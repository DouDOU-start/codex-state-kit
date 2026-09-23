use super::*;
use axum::routing::post;
use base64::Engine;

fn write_account(home: &Path, account: &str) {
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
        serde_json::json!({"chatgpt_account_id":account,"email":format!("{account}@example.com")})
            .to_string(),
    );
    std::fs::write(login::kit_auth_path(home), serde_json::json!({
        "auth_mode": "chatgpt", "tokens": {
            "id_token": format!("e30.{claims}.signature"),
            "access_token": format!("access-{account}"), "refresh_token": "test-refresh", "account_id": account
        }
    }).to_string()).unwrap();
}

fn ticket(marker: u8) -> String {
    let mut bytes = vec![marker; 219];
    bytes[0] = 0x80;
    bytes[1..9].copy_from_slice(&chrono::Utc::now().timestamp().to_be_bytes());
    base64::engine::general_purpose::URL_SAFE.encode(bytes)
}

fn request(account: Option<&str>, state: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method("POST")
        .uri("/responses")
        .header("authorization", "Bearer client-old-access")
        .header(turn_state::HEADER_NAME, state);
    if let Some(account) = account {
        request = request.header("chatgpt-account-id", account);
    }
    request
        .body(Body::from(r#"{"model":"switch-model"}"#))
        .unwrap()
}

#[tokio::test]
async fn account_switch_preserves_request_identity_and_rejects_old_tickets() {
    if std::env::var_os("CSK_ACCOUNT_SWITCH_TEST_CHILD").is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "proxy::account_switch_tests::account_switch_preserves_request_identity_and_rejects_old_tickets", "--nocapture"])
            .env("CSK_ACCOUNT_SWITCH_TEST_CHILD", "1")
            .env("HOME", home.path()).env("USERPROFILE", home.path())
            .env("APPDATA", home.path()).env("LOCALAPPDATA", home.path())
            .env_remove("HTTP_PROXY").env_remove("HTTPS_PROXY").env_remove("ALL_PROXY")
            .env_remove("http_proxy").env_remove("https_proxy").env_remove("all_proxy")
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    tokio::time::timeout(Duration::from_secs(15), async {
        let home = tempfile::tempdir().unwrap();
        let (send, mut requests) =
            tokio::sync::mpsc::channel::<(HeaderMap, oneshot::Sender<Response>)>(8);
        let router = axum::Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, _body: axum::body::Bytes| {
                let send = send.clone();
                async move {
                    let (reply, receive) = oneshot::channel();
                    send.send((headers, reply)).await.unwrap();
                    receive.await.unwrap()
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        write_account(home.path(), "account-a");
        let app = Arc::new(
            App::new(Settings {
                upstream: endpoint.clone(),
                outbound_proxy: endpoint,
                outbound_mode: OutboundMode::Manual,
                codex_home: home.path().display().to_string(),
                ..Settings::default()
            })
            .unwrap(),
        );
        let token_a = ticket(1);
        let token_b = ticket(2);
        app.sync_logged_in_account().await;
        assert!(app
            .turn_state
            .lock()
            .await
            .capture("switch-model", &token_a, "test"));

        // A remains in flight while B logs in; its log identity must stay A.
        let pending = tokio::spawn(proxy_http(
            app.clone(),
            request(Some("account-a"), &token_a),
        ));
        let (headers, reply) = requests.recv().await.unwrap();
        assert_eq!(headers["authorization"], "Bearer access-account-a");
        assert!(!headers.contains_key(turn_state::HEADER_NAME));
        let (finish_a, wait_a) = oneshot::channel();
        reply
            .send(Response::new(Body::from_stream(
                futures_util::stream::once(async move {
                    wait_a.await.unwrap();
                    Ok::<_, std::io::Error>("done-a")
                }),
            )))
            .unwrap();
        let response_a = pending.await.unwrap();

        // No status polling or pool synchronization yet: the pool still belongs to A.
        write_account(home.path(), "account-b");
        for client in [Some("account-a"), Some("account-b"), None] {
            let pending = tokio::spawn(proxy_http(app.clone(), request(client, &token_a)));
            let (headers, reply) = requests.recv().await.unwrap();
            assert_eq!(headers["authorization"], "Bearer access-account-b");
            assert_eq!(headers["chatgpt-account-id"], "account-b");
            assert!(!headers.contains_key(turn_state::HEADER_NAME));
            reply.send(Response::new(Body::from("done-b"))).unwrap();
            axum::body::to_bytes(pending.await.unwrap().into_body(), 1024)
                .await
                .unwrap();
            let entry = app.logs.lock().await.back().cloned().unwrap();
            assert_eq!(entry.account_id.as_deref(), Some("account-b"));
            assert_eq!(
                entry.account_email.as_deref(),
                Some("account-b@example.com")
            );
            assert_eq!(entry.turn_state_action, "not_applicable");
        }
        app.sync_logged_in_account().await;
        assert!(app
            .turn_state
            .lock()
            .await
            .peek_for_model("switch-model")
            .is_none());
        assert!(app
            .turn_state
            .lock()
            .await
            .capture("switch-model", &token_b, "test"));
        let pending = tokio::spawn(proxy_http(
            app.clone(),
            request(Some("account-a"), &token_a),
        ));
        let (headers, reply) = requests.recv().await.unwrap();
        assert!(!headers.contains_key(turn_state::HEADER_NAME));
        assert_eq!(headers["chatgpt-account-id"], "account-b");
        reply.send(Response::new(Body::from("done-b"))).unwrap();
        axum::body::to_bytes(pending.await.unwrap().into_body(), 1024)
            .await
            .unwrap();
        finish_a.send(()).unwrap();
        axum::body::to_bytes(response_a.into_body(), 1024)
            .await
            .unwrap();
        let logs = app.logs.lock().await;
        assert_eq!(
            logs.front().unwrap().account_id.as_deref(),
            Some("account-a")
        );
        assert_eq!(
            logs.front().unwrap().account_email.as_deref(),
            Some("account-a@example.com")
        );
        assert!(!logs.front().unwrap().in_progress);
        drop(logs);

        // A probe may finish after a login file changes and before the next status poll.
        write_account(home.path(), "account-a");
        app.sync_logged_in_account().await;
        let fetch_app = app.clone();
        let pending = tokio::spawn(async move { fetch_app.fetch_once("switch-model").await });
        let (headers, reply) = requests.recv().await.unwrap();
        assert_eq!(headers["chatgpt-account-id"], "account-a");
        write_account(home.path(), "account-b");
        let response = Response::builder()
            .header(turn_state::HEADER_NAME, &token_a)
            .body(Body::empty())
            .unwrap();
        reply.send(response).unwrap();
        assert_eq!(
            pending.await.unwrap().unwrap_err().retry,
            FetchRetryClass::Stale
        );
        assert!(app
            .turn_state
            .lock()
            .await
            .peek_for_model("switch-model")
            .is_none());
        let entry = app.logs.lock().await.back().cloned().unwrap();
        assert_eq!(entry.account_id.as_deref(), Some("account-a"));
        assert_eq!(
            entry.account_email.as_deref(),
            Some("account-a@example.com")
        );
        assert_eq!(entry.turn_state_action, "discarded_stale_account");
        let status = app.status().await;
        assert_eq!(status.current_account_id.as_deref(), Some("account-b"));
        assert_eq!(
            status.current_account_email.as_deref(),
            Some("account-b@example.com")
        );
        assert!(app.turn_state.lock().await.is_bound_to_account("account-b"));
        assert_eq!(status.account_traffic.concurrent_requests, 0);
        // An account switch must interrupt a probe's old
        // cooldown instead of waiting for that entire cooldown to elapse.
        app.defer_next_fetch(Duration::from_secs(30)).await;
        let probe_app = app.clone();
        let cooling_probe = tokio::spawn(async move { probe_app.fetch_once("switch-model").await });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if app.fetch_gate.try_lock().is_err() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        write_account(home.path(), "account-c");
        let switched = tokio::time::timeout(
            Duration::from_secs(2),
            app.sync_request_identity(home.path()),
        )
        .await;
        assert!(
            switched.is_ok(),
            "account switch is blocked behind the previous account's fetch cooldown"
        );
        assert_eq!(switched.unwrap().unwrap().0.account_id, "account-c");
        assert_eq!(
            cooling_probe.await.unwrap().unwrap_err().retry,
            FetchRetryClass::Stale
        );
        assert!(app.turn_state.lock().await.is_bound_to_account("account-c"));

        // Cancelling an identity switch while a probe is in flight must not
        // leave the generation odd and permanently disable future fetching.
        write_account(home.path(), "account-d");
        let gate = app.fetch_gate.lock().await;
        let switch_app = app.clone();
        let switch_home = home.path().to_path_buf();
        let switching =
            tokio::spawn(async move { switch_app.sync_request_identity(&switch_home).await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while app.fetch_generation.load(Ordering::SeqCst) % 2 == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        switching.abort();
        assert!(switching.await.unwrap_err().is_cancelled());
        assert_eq!(app.fetch_generation.load(Ordering::SeqCst) % 2, 0);
        drop(gate);
        assert_eq!(
            app.sync_request_identity(home.path())
                .await
                .unwrap()
                .0
                .account_id,
            "account-d"
        );
        server.abort();
    })
    .await
    .unwrap();
}
