use super::*;

const GATEWAY_KEY: &str = "csk_path_alias_test_key";
const MODELS_BODY: &str = r#"{"object":"list","data":[{"id":"alias-model"}]}"#;
const SSE_BODY: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_alias\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";

#[tokio::test]
async fn prefixed_and_unprefixed_requests_share_upstream_routes_and_lan_auth() {
    // Run in an isolated process so logging and account bookkeeping cannot
    // access the real Kit home or change other tests' process-wide state.
    if std::env::var_os("CSK_PATH_ALIAS_TEST_CHILD").is_none() {
        let root = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "proxy::path_alias_tests::prefixed_and_unprefixed_requests_share_upstream_routes_and_lan_auth",
                "--nocapture",
            ])
            .env("CSK_PATH_ALIAS_TEST_CHILD", "1")
            .env("HOME", root.path())
            .env("USERPROFILE", root.path())
            .env("APPDATA", root.path())
            .env("LOCALAPPDATA", root.path())
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("all_proxy")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }

    tokio::time::timeout(Duration::from_secs(20), async {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            login::kit_auth_path(home.path()),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"alias-access-token","refresh_token":"alias-refresh-token","account_id":"alias-account"}}"#,
        )
        .unwrap();

        let (send, mut observed) = tokio::sync::mpsc::channel(16);
        let router = axum::Router::new().fallback(move |request: Request<Body>| {
            let send = send.clone();
            async move {
                let (parts, body) = request.into_parts();
                let body = axum::body::to_bytes(body, 4096).await.unwrap();
                let is_models = parts.uri.path().ends_with("/models");
                send.send((parts, body)).await.unwrap();
                if is_models {
                    ([(header::CONTENT_TYPE, "application/json")], MODELS_BODY)
                        .into_response()
                } else {
                    // Multiple body chunks exercise the HTTP SSE forwarding path.
                    let split = SSE_BODY.len() / 2;
                    let chunks = [
                        Ok::<_, std::io::Error>(&SSE_BODY[..split]),
                        Ok(&SSE_BODY[split..]),
                    ];
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(futures_util::stream::iter(chunks)),
                    )
                        .into_response()
                }
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });

        for upstream_prefix in ["/backend-api/codex", "/v1"] {
            for lan_enabled in [false, true] {
                let app = Arc::new(
                    App::new(Settings {
                        upstream: format!("{endpoint}{upstream_prefix}"),
                        codex_home: home.path().display().to_string(),
                        // The loopback mock also acts as a manual HTTP proxy,
                        // keeping this test independent of host proxy settings.
                        outbound_proxy: endpoint.clone(),
                        outbound_mode: OutboundMode::Manual,
                        chain_system_proxy: false,
                        lan_access_enabled: lan_enabled,
                        lan_api_key_hash: lan_api_key_hash(GATEWAY_KEY),
                        ..Settings::default()
                    })
                    .unwrap(),
                );

                for downstream_prefix in ["", "/v1"] {
                    for operation in ["models", "responses"] {
                        let path = format!(
                            "{downstream_prefix}/{operation}?client_version=1.2.3&value=a%2Fb"
                        );
                        let method = if operation == "models" { "GET" } else { "POST" };

                        if lan_enabled {
                            let rejected = proxy(
                                State(app.clone()),
                                Request::builder()
                                    .method(method)
                                    .uri(&path)
                                    .body(Body::empty())
                                    .unwrap(),
                            )
                            .await;
                            assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
                            assert!(observed.try_recv().is_err());
                        }

                        let mut request = Request::builder()
                            .method(method)
                            .uri(&path)
                            .header(header::CONTENT_TYPE, "application/json")
                            .header("chatgpt-account-id", "remote-account");
                        if lan_enabled {
                            // Cover both supported gateway-key header styles.
                            request = if operation == "models" {
                                request.header(
                                    header::AUTHORIZATION,
                                    format!("Bearer {GATEWAY_KEY}"),
                                )
                            } else {
                                request
                                    .header("x-api-key", GATEWAY_KEY)
                                    .header("api-key", "must-not-leak")
                            };
                        }
                        if operation == "responses" {
                            request = request.header(header::ACCEPT, "text/event-stream");
                        }
                        let request_body = if operation == "models" {
                            Body::empty()
                        } else {
                            Body::from(r#"{"model":"alias-model","input":[],"stream":true}"#)
                        };
                        let response = proxy(
                            State(app.clone()),
                            request.body(request_body).unwrap(),
                        )
                        .await;
                        assert_eq!(response.status(), StatusCode::OK, "{path}");
                        assert_eq!(
                            response.headers()[header::CONTENT_TYPE],
                            if operation == "models" { "application/json" } else { "text/event-stream" }
                        );
                        let body = axum::body::to_bytes(response.into_body(), 4096)
                            .await
                            .unwrap();
                        assert_eq!(
                            body.as_ref(),
                            if operation == "models" { MODELS_BODY.as_bytes() } else { SSE_BODY.as_bytes() }
                        );

                        let (parts, upstream_body) = observed.recv().await.unwrap();
                        assert_eq!(parts.method.as_str(), method);
                        assert_eq!(
                            parts.uri.path_and_query().unwrap().as_str(),
                            format!("{upstream_prefix}/{operation}?client_version=1.2.3&value=a%2Fb")
                        );
                        assert_eq!(parts.headers[header::AUTHORIZATION], "Bearer alias-access-token");
                        assert_eq!(parts.headers["chatgpt-account-id"], "alias-account");
                        assert!(!parts.headers.contains_key("x-api-key"));
                        assert!(!parts.headers.contains_key("api-key"));
                        assert!(!format!("{:?}", parts.headers).contains(GATEWAY_KEY));
                        if operation == "responses" {
                            let json: serde_json::Value = serde_json::from_slice(&upstream_body).unwrap();
                            assert_eq!(json["model"], "alias-model");
                            assert_eq!(json["stream"], true);
                        }
                    }
                }
            }
        }
        server.abort();
    })
    .await
    .unwrap();
}
