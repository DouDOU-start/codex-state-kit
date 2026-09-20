use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_headers(socket: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let byte = tokio::time::timeout(Duration::from_secs(5), socket.read_u8())
            .await
            .unwrap()
            .unwrap();
        bytes.push(byte);
        assert!(bytes.len() < 16_384);
    }
    String::from_utf8(bytes).unwrap()
}

// A controlled proxy response lets us hold an SSE stream open across a settings update.
async fn streaming_proxy() -> (
    String,
    oneshot::Receiver<String>,
    oneshot::Sender<()>,
    JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (headers_tx, headers_rx) = oneshot::channel();
    let (finish_tx, finish_rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        headers_tx.send(read_headers(&mut socket).await).unwrap();
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\ndata: 1\n\n\r\n").await.unwrap();
        let _ = finish_rx.await;
        socket
            .write_all(b"9\r\ndata: 2\n\n\r\n0\r\n\r\n")
            .await
            .unwrap();
    });
    (url, headers_rx, finish_tx, task)
}

async fn fixed_response_proxy(
    status: &str,
    content_type: &str,
    body: &'static [u8],
) -> (String, oneshot::Receiver<String>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (headers_tx, headers_rx) = oneshot::channel();
    let status = status.to_string();
    let content_type = content_type.to_string();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        headers_tx.send(read_headers(&mut socket).await).unwrap();
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        socket.write_all(body).await.unwrap();
    });
    (url, headers_rx, task)
}

fn patch(settings: &Settings, proxy: &str) -> SettingsPatch {
    SettingsPatch {
        token_reuse_policy: settings.token_reuse_policy,
        state_miss_policy: settings.state_miss_policy,
        proxy_listen: settings.proxy_listen.clone(),
        upstream: settings.upstream.clone(),
        codex_home: settings.codex_home.clone(),
        outbound_proxy: settings.outbound_proxy.clone(),
        upstream_proxy: proxy.into(),
        outbound_mode: settings.outbound_mode,
        warp_http2: settings.warp_http2,
        models: settings.models.clone(),
    }
}

#[tokio::test]
async fn upstream_proxy_hot_update_and_failures() {
    // Isolate all application persistence without modifying the test runner's environment.
    if std::env::var_os("CSK_UPSTREAM_TEST_CHILD").is_none() {
        let dir = tempfile::tempdir().unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "proxy::upstream_proxy_tests::upstream_proxy_hot_update_and_failures",
                "--nocapture",
            ])
            .env("CSK_UPSTREAM_TEST_CHILD", "1")
            .env("HOME", dir.path())
            .env("USERPROFILE", dir.path())
            .env("APPDATA", dir.path())
            .env("LOCALAPPDATA", dir.path())
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("all_proxy")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        let (first, first_headers, finish_first, first_task) = streaming_proxy().await;
        let settings = Settings {
            upstream: "http://upstream.invalid".into(),
            upstream_proxy: first.clone(),
            outbound_proxy: "http://127.0.0.1:9".into(),
            codex_home: crate::settings::home_dir().display().to_string(),
            ..Settings::default()
        };
        let app = Arc::new(App::new(settings.clone()).unwrap());
        let handle = ProxyHandle::new(app.clone());
        app.turn_state.lock().await.register_model("test-model");
        use base64::Engine;
        let mut token_bytes = vec![0u8; 219];
        token_bytes[0] = 0x80;
        token_bytes[1..9].copy_from_slice(&chrono::Utc::now().timestamp().to_be_bytes());
        let token = base64::engine::general_purpose::URL_SAFE.encode(token_bytes);
        assert!(app
            .turn_state
            .lock()
            .await
            .capture("test-model", &token, "test"));
        let token_before = TurnStateStore::load().peek_for_model("test-model");
        let warp_before = app.warp.status().phase;
        let mut route_details = NetworkLogDetails::default();
        let response = forward_http_with_log(
            &app,
            Request::builder()
                .uri("/events?a=1")
                .header("proxy-authorization", "must-not-leak")
                .body(Body::empty())
                .unwrap(),
            &mut route_details,
            Instant::now(),
        )
        .await
        .unwrap();
        assert_eq!(route_details.route_kind, logs::ROUTE_EXPLICIT_PROXY);
        assert_eq!(
            route_details.proxy_endpoint.as_deref(),
            Some(first.as_str())
        );
        assert_eq!(route_details.target_origin, "http://upstream.invalid:80");
        assert_eq!(
            route_details.final_origin.as_deref(),
            Some("http://upstream.invalid:80")
        );
        assert_eq!(
            route_details.peer_addr.as_deref(),
            first.strip_prefix("http://")
        );
        let headers = first_headers.await.unwrap().to_lowercase();
        assert!(headers.starts_with("get http://upstream.invalid/events?a=1 http/1.1"));
        assert!(!headers.contains("proxy-authorization"));
        use futures_util::StreamExt;
        let mut stream = response.into_body().into_data_stream();
        assert_eq!(&stream.next().await.unwrap().unwrap()[..], b"data: 1\n\n");
        let lifecycle = route_details.stream_lifecycle.clone().unwrap();
        let active = lifecycle.snapshot();
        assert_eq!(active.state, "streaming");
        assert_eq!(active.stream_chunks, 1);
        assert_eq!(active.stream_bytes, 9);

        let (second, second_headers, finish_second, second_task) = streaming_proxy().await;
        handle
            .apply_settings(patch(&settings, &second))
            .await
            .unwrap();
        assert_eq!(crate::settings::load_settings().upstream_proxy, second);
        assert_eq!(
            app.settings.lock().await.outbound_proxy,
            settings.outbound_proxy
        );
        assert_eq!(
            app.turn_state.lock().await.peek_for_model("test-model"),
            Some(token)
        );
        assert_eq!(
            TurnStateStore::load().peek_for_model("test-model"),
            token_before
        );
        assert_eq!(app.warp.status().phase, warp_before);
        assert!(handle.task.lock().await.is_none());
        assert!(handle.fetch_task.lock().await.is_none());
        let response = forward_http(
            &app,
            Request::builder().uri("/new").body(Body::empty()).unwrap(),
        )
        .await
        .unwrap();
        assert!(second_headers
            .await
            .unwrap()
            .contains("http://upstream.invalid/new"));
        finish_second.send(()).unwrap();
        assert_eq!(
            &axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()[..],
            b"data: 1\n\ndata: 2\n\n"
        );
        finish_first.send(()).unwrap();
        assert_eq!(&stream.next().await.unwrap().unwrap()[..], b"data: 2\n\n");
        assert!(stream.next().await.is_none());
        let completed = lifecycle.snapshot();
        assert_eq!(completed.state, "completed");
        assert_eq!(completed.stream_chunks, 2);
        assert_eq!(completed.stream_bytes, 18);
        first_task.await.unwrap();
        second_task.await.unwrap();

        assert!(handle
            .apply_settings(patch(&settings, "ftp://user:secret@localhost"))
            .await
            .is_err());
        assert_eq!(app.settings.lock().await.upstream_proxy, second);
        assert_eq!(crate::settings::load_settings().upstream_proxy, second);
        // A persistence failure must also keep the old runtime client/configuration.
        let path = crate::settings::settings_path();
        let saved = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(handle
            .apply_settings(patch(&settings, "http://127.0.0.1:9"))
            .await
            .is_err());
        assert_eq!(app.settings.lock().await.upstream_proxy, second);
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, saved).unwrap();

        // If the selected proxy is dead, a reachable upstream must not be contacted directly.
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_proxy = format!("http://user:secret@{}", closed.local_addr().unwrap());
        drop(closed);
        handle
            .apply_settings(patch(&settings, &dead_proxy))
            .await
            .unwrap();
        app.settings.lock().await.upstream = format!("http://{}", upstream.local_addr().unwrap());
        let response = proxy_http(
            app.clone(),
            Request::builder()
                .uri("/check?token=must-not-enter-log")
                .header("chatgpt-account-id", "failed-account")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            app.traffic.view(Some("failed-account"), Instant::now()),
            AccountTraffic {
                concurrent_requests: 0,
                rpm: 1
            }
        );
        let failed_log = app.logs.lock().await.back().cloned().unwrap();
        assert_eq!(failed_log.route_kind, logs::ROUTE_EXPLICIT_PROXY);
        assert_eq!(failed_log.path, "/check");
        assert_eq!(failed_log.error_kind.as_deref(), Some("connect"));
        let serialized = serde_json::to_string(&failed_log).unwrap();
        assert!(!serialized.contains("user"));
        assert!(!serialized.contains("secret"));
        assert!(!serialized.contains("must-not-enter-log"));
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&body).contains("secret"));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), upstream.accept())
                .await
                .is_err()
        );
        handle.apply_settings(patch(&settings, "")).await.unwrap();
        assert!(crate::settings::load_settings().upstream_proxy.is_empty());
        assert!(App::new(crate::settings::load_settings())
            .unwrap()
            .settings
            .lock()
            .await
            .upstream_proxy
            .is_empty());
        verify_response_metrics().await;
    })
    .await
    .unwrap();
}

// Runs inside the isolated HOME/proxy environment above; never calls a public upstream.
async fn verify_response_metrics() {
    use futures_util::StreamExt;
    for outcome in [
        "complete",
        "length",
        "zstd",
        "zstd_length",
        "no_content_type",
        "no_content_type_completed_drop",
        "cancel",
        "body_error",
        "cancel_headers",
    ] {
        let preamble = b"data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.6-sol\"}}\n\n";
        let delta = b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n";
        let end = b"data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-6-sol\",\"usage\":{\"output_tokens\":120}}}\n\ndata: [DONE]\n\n";
        let compressed = outcome.starts_with("zstd");
        let (preamble, delta, end) = if compressed {
            use std::io::Write;
            let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
            encoder.write_all(preamble).unwrap();
            encoder.flush().unwrap();
            let preamble = std::mem::take(encoder.get_mut());
            encoder.write_all(delta).unwrap();
            encoder.flush().unwrap();
            let delta = std::mem::take(encoder.get_mut());
            encoder.write_all(end).unwrap();
            (preamble, delta, encoder.finish().unwrap())
        } else {
            (preamble.to_vec(), delta.to_vec(), end.to_vec())
        };
        let body_length = preamble.len() + delta.len() + end.len();
        let fixed_length = outcome.ends_with("length");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (send, mut recv) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let (received_tx, received_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let app = Arc::new(
            App::new(Settings {
                upstream: format!("http://{}", listener.local_addr().unwrap()),
                upstream_proxy: String::new(),
                codex_home: crate::settings::home_dir().display().to_string(),
                ..Settings::default()
            })
            .unwrap(),
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_headers(&mut socket).await;
            received_tx.send(()).unwrap();
            if release_rx.await.is_err() {
                return;
            }
            let framing = if fixed_length {
                format!(
                    "Content-Length: {}",
                    body_length
                )
            } else {
                "Transfer-Encoding: chunked".into()
            };
            let encoding = if compressed { "Content-Encoding: zstd\r\n" } else { "" };
            let content_type = if outcome.starts_with("no_content_type") { "" } else { "Content-Type: text/event-stream\r\n" };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\n{content_type}{encoding}{framing}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            while let Some(bytes) = recv.recv().await {
                if bytes.is_empty() {
                    return;
                } // Intentional premature upstream EOF.
                if !fixed_length {
                    socket
                        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
                        .await
                        .unwrap();
                }
                socket.write_all(&bytes).await.unwrap();
                if !fixed_length {
                    socket.write_all(b"\r\n").await.unwrap();
                }
            }
            if !fixed_length {
                let _ = socket.write_all(b"0\r\n\r\n").await;
            }
        });
        let request = tokio::spawn(proxy_http(
            app.clone(),
            Request::builder()
                .uri("/metrics")
                .header("accept", "text/event-stream")
                .header("chatgpt-account-id", "metrics-account")
                .body(Body::empty())
                .unwrap(),
        ));
        received_rx.await.unwrap();
        assert_eq!(
            app.traffic.view(Some("metrics-account"), Instant::now()),
            AccountTraffic {
                concurrent_requests: 1,
                rpm: 1
            }
        );
        if outcome == "cancel_headers" {
            request.abort();
            assert!(request.await.unwrap_err().is_cancelled());
            assert_eq!(
                app.traffic.view(Some("metrics-account"), Instant::now()),
                AccountTraffic {
                    concurrent_requests: 0,
                    rpm: 1
                }
            );
            drop(release_tx);
            server.await.unwrap();
            continue;
        }
        release_tx.send(()).unwrap();
        let response = request.await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        if compressed {
            assert_eq!(response.headers()[header::CONTENT_ENCODING], "zstd");
        }
        let initial = app.logs.lock().await.back().unwrap().snapshot();
        assert!(initial.in_progress);
        assert!(initial.first_token_ms.is_none());
        assert_eq!(initial.stream_state, "awaiting_first_chunk");
        assert_eq!(initial.stream_chunks, 0);
        let mut stream = response.into_body().into_data_stream();
        send.send(preamble.to_vec()).await.unwrap();
        assert_eq!(&stream.next().await.unwrap().unwrap()[..], preamble);
        let preamble_log = app.logs.lock().await.back().unwrap().snapshot();
        assert!(preamble_log.first_token_ms.is_none());
        assert_eq!(preamble_log.stream_state, "streaming");
        assert!(preamble_log.stream_chunks >= 1);
        assert_eq!(preamble_log.upstream_response_model.as_deref(), Some("gpt-5.6-sol"));
        tokio::time::sleep(Duration::from_millis(25)).await;
        send.send(delta.to_vec()).await.unwrap();
        assert_eq!(&stream.next().await.unwrap().unwrap()[..], delta);
        let live = app.logs.lock().await.back().unwrap().snapshot();
        assert!(live.in_progress);
        assert_eq!(
            app.traffic
                .view(Some("metrics-account"), Instant::now())
                .concurrent_requests,
            1
        );
        assert!(live.first_token_ms.unwrap() >= initial.response_header_ms.unwrap() + 20);
        assert!(live.tokens_per_second.is_none());
        tokio::time::sleep(Duration::from_millis(25)).await;
        match outcome {
            "complete" | "length" | "zstd" | "zstd_length" | "no_content_type" | "no_content_type_completed_drop" => {
                send.send(end.to_vec()).await.unwrap();
                let mut forwarded = Vec::new();
                if outcome.ends_with("completed_drop") {
                    // Codex stops reading at response.completed, before HTTP EOF.
                    forwarded.extend_from_slice(&stream.next().await.unwrap().unwrap());
                    drop(stream);
                    tokio::time::timeout(Duration::from_secs(1), async {
                        while app.logs.lock().await.back().unwrap().in_progress {
                            tokio::task::yield_now().await;
                        }
                    }).await.unwrap();
                    drop(send);
                } else {
                    drop(send);
                    while let Some(bytes) = stream.next().await {
                        forwarded.extend_from_slice(&bytes.unwrap());
                    }
                }
                assert_eq!(forwarded, end);
                let logs = app.logs.lock().await;
                assert_eq!(logs.len(), 1);
                let entry = logs.back().unwrap().snapshot();
                assert!(!entry.in_progress);
                assert_eq!(entry.id, initial.id);
                assert_eq!(entry.output_tokens, Some(120));
                assert_eq!(entry.upstream_response_model.as_deref(), Some("gpt-6-sol"));
                assert_eq!(serde_json::to_value(&entry).unwrap()["upstreamResponseModel"], "gpt-6-sol");
                assert!(entry.ms > entry.first_token_ms.unwrap());
                assert_eq!(
                    entry.tokens_per_second,
                    Some(120000.0 / (entry.ms - entry.first_token_ms.unwrap()) as f64)
                );
                assert!(entry.error_kind.is_none());
                assert_eq!(entry.stream_state, "completed");
                assert!(entry.stream_total_ms.is_some());
                assert!(entry.stream_bytes > 0);
            }
            "cancel" => {
                drop(stream);
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        if !app.logs.lock().await.back().unwrap().in_progress {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                let entry = app.logs.lock().await.back().unwrap().snapshot();
                assert_eq!(entry.error_kind.as_deref(), Some("client_cancelled"));
                assert_eq!(entry.upstream_response_model.as_deref(), Some("gpt-5.6-sol"));
                assert!(entry.first_token_ms.is_some());
                assert!(entry.tokens_per_second.is_none());
                assert_eq!(entry.stream_state, "cancelled");
                drop(send);
            }
            _ => {
                send.send(Vec::new()).await.unwrap();
                assert!(stream.next().await.unwrap().is_err());
                let entry = app.logs.lock().await.back().unwrap().snapshot();
                assert!(!entry.in_progress);
                assert_eq!(entry.error_kind.as_deref(), Some("response_body"));
                assert!(entry.tokens_per_second.is_none());
                assert_eq!(entry.stream_state, "error");
            }
        }
        server.await.unwrap();
        assert_eq!(
            app.traffic.view(Some("metrics-account"), Instant::now()),
            AccountTraffic {
                concurrent_requests: 0,
                rpm: 1
            }
        );
    }
}

#[tokio::test]
async fn upstream_proxy_https_connect_uses_proxy_auth_then_tls() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://user:secret@{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let headers = read_headers(&mut socket).await.to_lowercase();
            assert!(headers.starts_with("connect upstream.invalid:443 http/1.1"));
            assert!(headers.contains("proxy-authorization: basic dxnlcjpzzwnyzxq="));
            socket
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            // The tunnel carries a TLS handshake, not plaintext proxy credentials.
            assert_eq!(socket.read_u8().await.unwrap(), 0x16);
        });
        // The mock ends after ClientHello; no public service or certificate bypass is needed.
        assert!(upstream_http_client(&proxy)
            .unwrap()
            .get("https://upstream.invalid/events")
            .send()
            .await
            .is_err());
        task.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn json_error_response_is_not_reported_as_sse() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let home = tempfile::tempdir().unwrap();
        let (proxy, headers, task) =
            fixed_response_proxy("400 Bad Request", "application/json", b"{}").await;
        let app = App::new(Settings {
            upstream: "http://upstream.invalid".into(),
            upstream_proxy: proxy,
            codex_home: home.path().display().to_string(),
            ..Settings::default()
        })
        .unwrap();
        let mut details = NetworkLogDetails::default();
        let response = forward_http_with_log(
            &app,
            Request::builder()
                .uri("/responses")
                .header("accept", "text/event-stream")
                .body(Body::empty())
                .unwrap(),
            &mut details,
            Instant::now(),
        )
        .await
        .unwrap();
        assert!(headers.await.unwrap().contains("http://upstream.invalid/responses"));
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(details.transport, "http");
        assert!(details.stream_lifecycle.is_none());
        assert_eq!(
            &axum::body::to_bytes(response.into_body(), 16).await.unwrap()[..],
            b"{}"
        );
        task.await.unwrap();
    })
    .await
    .unwrap();
}
