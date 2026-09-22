use super::*;
use axum::body::Bytes;
use base64::Engine;
use tokio::io::AsyncWriteExt;

fn write_login(home: &Path, account: &str) {
    std::fs::write(
        login::kit_auth_path(home),
        serde_json::json!({
            "auth_mode":"chatgpt", "tokens":{
                "access_token":"test-access", "refresh_token":"test-refresh", "account_id":account
            }
        })
        .to_string(),
    )
    .unwrap();
}

fn ticket(age: i64) -> String {
    let mut bytes = vec![5; 219];
    bytes[0] = 0x80;
    bytes[1..9].copy_from_slice(&(chrono::Utc::now().timestamp() - age).to_be_bytes());
    base64::engine::general_purpose::URL_SAFE.encode(bytes)
}

fn request(state: bool, model: bool) -> Request<Body> {
    request_body(
        state,
        if model {
            r#"{"model":"policy-model"}"#
        } else {
            "{}"
        },
    )
}

fn follow_up_request(state: bool) -> Request<Body> {
    request_body(
        state,
        r#"{"model":"policy-model","previous_response_id":"resp_1","input":[{"type":"function_call_output","call_id":"c1","output":"ok"}]}"#,
    )
}

fn request_body(state: bool, body: &'static str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/responses")
        .header("chatgpt-account-id", "account-a");
    if state {
        builder = builder.header(turn_state::HEADER_NAME, "client-state");
    }
    builder.body(Body::from(body)).unwrap()
}

#[tokio::test]
async fn policies_preserve_strip_wait_and_cancel_without_cross_account_replay() {
    if std::env::var_os("CSK_POLICY_TEST_CHILD").is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "proxy::state_policy_tests::policies_preserve_strip_wait_and_cancel_without_cross_account_replay", "--nocapture"])
            .env("CSK_POLICY_TEST_CHILD", "1")
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
    tokio::time::timeout(Duration::from_secs(20), async {
        let home = tempfile::tempdir().unwrap();
        write_login(home.path(), "account-a");
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel::<(HeaderMap, Vec<u8>)>();
        let upstream = axum::Router::new().fallback(move |headers: HeaderMap, body: Bytes| {
            let sent = sent.clone();
            async move {
                sent.send((headers, body.to_vec())).unwrap();
                "ok"
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Arc::new(App::new(Settings {
            token_reuse_policy: TokenReusePolicy::PerModel,
            network_route_policy: NetworkRoutePolicy::Separate,
            upstream: format!("http://{}", listener.local_addr().unwrap()),
            codex_home: home.path().display().to_string(),
            ..Settings::default()
        }).unwrap());
        app.sync_logged_in_account().await;
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap(); });
        let fresh = ticket(0);
        use StateMissPolicy::*;
        for (policy, cached, model, has_state, expected, action) in [
            (Preserve, false, true, true, Some("client-state"), "preserved_no_ticket"),
            (Strip, false, true, true, None, "removed_by_policy"),
            (Preserve, false, false, true, Some("client-state"), "preserved_unknown_model"),
            (Strip, false, false, true, None, "removed_by_policy"),
            (Preserve, true, true, true, Some(fresh.as_str()), "replaced"),
            (Wait, true, true, true, Some(fresh.as_str()), "replaced"),
            (Strip, true, true, true, Some(fresh.as_str()), "replaced"),
            (Preserve, true, true, false, Some(fresh.as_str()), "injected"),
            (Wait, true, true, false, Some(fresh.as_str()), "injected"),
            (Strip, true, true, false, Some(fresh.as_str()), "injected"),
            (Preserve, false, true, false, None, "initial_request"),
            (Passthrough, true, true, true, Some("client-state"), "preserved_by_policy"),
            (StripAll, true, true, true, None, "removed_all_policy"),
            (Passthrough, false, true, false, None, "initial_request"),
            (StripAll, false, false, false, None, "removed_all_policy"),
        ] {
            app.settings.lock().await.state_miss_policy = policy;
            app.turn_state.lock().await.invalidate_all();
            if cached { assert!(app.turn_state.lock().await.capture("policy-model", &fresh, "test")); }
            let response = proxy_http(app.clone(), request(has_state, model)).await;
            assert_eq!(response.status(), StatusCode::OK, "{policy:?}");
            axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
            let (headers, _) = received.recv().await.unwrap();
            assert_eq!(headers.get(turn_state::HEADER_NAME).and_then(|v| v.to_str().ok()), expected, "{policy:?}");
            let log = app.logs.lock().await.back().cloned().unwrap();
            assert_eq!(log.turn_state_action, action);
            assert_eq!(log.state_policy, Some(policy));
        }
        for (policy, action) in [(Preserve, "header_only"), (Wait, "header_only"), (Strip, "header_only")] {
            app.settings.lock().await.state_miss_policy = policy;
            app.turn_state.lock().await.invalidate_all();
            assert!(app.turn_state.lock().await.capture("policy-model", &fresh, "test"));
            let response = proxy_http(app.clone(), follow_up_request(false)).await;
            assert_eq!(response.status(), StatusCode::OK, "{policy:?}");
            axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
            let (headers, body) = received.recv().await.unwrap();
            assert_eq!(
                headers.get(turn_state::HEADER_NAME).and_then(|v| v.to_str().ok()),
                Some(fresh.as_str()),
                "{policy:?}"
            );
            let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value.get("previous_response_id").and_then(|v| v.as_str()), Some("resp_1"), "{policy:?}");
            assert!(value.get("client_metadata").is_none(), "{policy:?}");
            assert_eq!(app.logs.lock().await.back().unwrap().turn_state_action, action);
        }
        app.settings.lock().await.state_miss_policy = Preserve;
        app.turn_state.lock().await.invalidate_all();
        assert!(app.turn_state.lock().await.capture("policy-model", &fresh, "test"));
        let response = proxy_http(app.clone(), request_body(
            false,
            r#"{"model":"policy-model","previous_response_id":"resp_client"}"#,
        )).await;
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let (headers, body) = received.recv().await.unwrap();
        assert_eq!(headers.get(turn_state::HEADER_NAME).and_then(|v| v.to_str().ok()), Some(fresh.as_str()));
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value.get("previous_response_id").and_then(|v| v.as_str()), Some("resp_client"));
        assert!(value.get("client_metadata").is_none());
        assert_eq!(app.logs.lock().await.back().unwrap().turn_state_action, "header_only");

        let response = proxy_http(app.clone(), request_body(
            false,
            r#"{"model":"policy-model","input":[{"type":"function_call_output","call_id":"c1","output":"ok"}]}"#,
        )).await;
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let (headers, body) = received.recv().await.unwrap();
        assert_eq!(headers.get(turn_state::HEADER_NAME).and_then(|v| v.to_str().ok()), Some(fresh.as_str()));
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["input"][0]["type"], "function_call_output");
        assert!(value.get("client_metadata").is_none());
        assert_eq!(app.logs.lock().await.back().unwrap().turn_state_action, "header_only");

        // 同轮且客户端已带 State：同样只换请求头，保留 previous_response_id。
        let response = proxy_http(app.clone(), follow_up_request(true)).await;
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let (headers, body) = received.recv().await.unwrap();
        assert_eq!(headers.get(turn_state::HEADER_NAME).and_then(|v| v.to_str().ok()), Some(fresh.as_str()));
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value.get("previous_response_id").and_then(|v| v.as_str()), Some("resp_1"));
        assert!(value.get("client_metadata").is_none());
        assert_eq!(app.logs.lock().await.back().unwrap().turn_state_action, "header_only");
        // Strip-all also applies to routes other than /responses.
        app.settings.lock().await.state_miss_policy = StripAll;
        let response = proxy_http(app.clone(), Request::builder().uri("/models")
            .header(turn_state::HEADER_NAME, "client-state").body(Body::empty()).unwrap()).await;
        axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        assert!(!received.recv().await.unwrap().0.contains_key(turn_state::HEADER_NAME));

        // Reject another account before Kit header override or waiting.
        // Same account with a stale Bearer is not a conflict after AT refresh.
        for kit_override in [true, false] {
            if !kit_override {
                std::fs::rename(login::kit_auth_path(home.path()), home.path().join("auth.json")).unwrap();
            }
            app.settings.lock().await.state_miss_policy = Wait;
            for cached in [false, true] {
                app.turn_state.lock().await.invalidate_all();
                if cached { app.turn_state.lock().await.capture("policy-model", &fresh, "test"); }
                for has_state in [false, true] {
                    let mut req = request(has_state, true);
                    req.headers_mut().insert("chatgpt-account-id", HeaderValue::from_static("account-b"));
                    req.headers_mut().insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer other-account-token"));
                    let traffic_before = app.traffic.view(Some("account-a"), Instant::now());
                    let response = tokio::time::timeout(Duration::from_secs(1), proxy_http(app.clone(), req))
                        .await.expect("account conflicts must return immediately, not wait");
                    assert_eq!(response.status(), StatusCode::CONFLICT);
                    let body = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
                    assert!(String::from_utf8_lossy(&body).contains("账号凭据不匹配"));
                    assert!(received.try_recv().is_err(), "a rejected request reached upstream");
                    assert_eq!(app.traffic.view(Some("account-a"), Instant::now()), traffic_before);
                    let logs = app.logs.lock().await;
                    let entry = logs.back().unwrap();
                    assert_eq!(entry.error_kind.as_deref(), Some("state_account_mismatch"));
                    assert!(!serde_json::to_string(entry).unwrap().contains("other-account-token"));
                }
            }
            app.turn_state.lock().await.invalidate_all();
            assert!(app.turn_state.lock().await.capture("policy-model", &fresh, "test"));
            let mut stale_same_account = request(true, true);
            stale_same_account
                .headers_mut()
                .insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer stale-access"));
            let response = tokio::time::timeout(Duration::from_secs(1), proxy_http(app.clone(), stale_same_account))
                .await
                .expect("same-account stale AT must not 409");
            assert_eq!(response.status(), StatusCode::OK, "kit_override={kit_override}");
            axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
            assert_eq!(received.recv().await.unwrap().0[turn_state::HEADER_NAME], fresh);
            if kit_override {
                let mut stale_without_account = request(true, true);
                stale_without_account.headers_mut().remove("chatgpt-account-id");
                stale_without_account
                    .headers_mut()
                    .insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer stale-access"));
                let response = tokio::time::timeout(Duration::from_secs(1), proxy_http(app.clone(), stale_without_account))
                    .await
                    .expect("Kit override can fill a missing account header");
                assert_eq!(response.status(), StatusCode::OK);
                axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
                assert_eq!(received.recv().await.unwrap().0[turn_state::HEADER_NAME], fresh);
            }
            // Matching credentials still forward successfully in both modes.
            app.turn_state.lock().await.capture("policy-model", &fresh, "test");
            let mut req = request(true, true);
            req.headers_mut().insert(header::AUTHORIZATION, HeaderValue::from_static("Bearer test-access"));
            let response = proxy_http(app.clone(), req).await;
            assert_eq!(response.status(), StatusCode::OK);
            axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
            assert_eq!(received.recv().await.unwrap().0[turn_state::HEADER_NAME], fresh);
        }
        write_login(home.path(), "account-a");
        // Absent identity is allowed when Kit supplies it from its own login.
        let mut req = request(true, true);
        req.headers_mut().remove("chatgpt-account-id");
        let response = proxy_http(app.clone(), req).await;
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let (headers, _) = received.recv().await.unwrap();
        assert_eq!(headers["authorization"], "Bearer test-access");
        assert_eq!(headers[turn_state::HEADER_NAME], fresh);

        app.settings.lock().await.state_miss_policy = Wait;
        let response = proxy_http(app.clone(), request(true, false)).await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(app.logs.lock().await.back().unwrap().error_kind.as_deref(), Some("state_model_unknown"));
        assert!(received.try_recv().is_err());

        // A waiting request ignores expired and other-model tickets, then uses
        // the matching ticket when it arrives. No probe is spawned by the waiter.
        app.turn_state.lock().await.invalidate_all();
        app.turn_state.lock().await.capture("policy-model", &ticket(2500), "test");
        app.turn_state.lock().await.capture("other-model", &fresh, "test");
        let pending = tokio::spawn(proxy_http(app.clone(), request(true, true)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!pending.is_finished());
        assert!(received.try_recv().is_err());
        app.turn_state.lock().await.capture("policy-model", &fresh, "test");
        let response = pending.await.unwrap();
        axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(received.recv().await.unwrap().0[turn_state::HEADER_NAME], fresh);
        assert_eq!(app.logs.lock().await.back().unwrap().turn_state_action, "replaced_after_wait");

        app.turn_state.lock().await.invalidate_all();
        let pending = tokio::spawn(proxy_http(app.clone(), follow_up_request(false)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!pending.is_finished());
        assert!(received.try_recv().is_err());
        app.turn_state.lock().await.capture("policy-model", &fresh, "test");
        let response = pending.await.unwrap();
        axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let (headers, body) = received.recv().await.unwrap();
        assert_eq!(headers[turn_state::HEADER_NAME], fresh);
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value.get("previous_response_id").and_then(|v| v.as_str()), Some("resp_1"));
        assert!(value.get("client_metadata").is_none());
        assert_eq!(app.logs.lock().await.back().unwrap().turn_state_action, "header_only_after_wait");

        app.turn_state.lock().await.invalidate_all();
        let pending = tokio::spawn(proxy_http(app.clone(), follow_up_request(true)));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!pending.is_finished());
        assert!(received.try_recv().is_err());
        app.turn_state.lock().await.capture("policy-model", &fresh, "test");
        let response = pending.await.unwrap();
        axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
        let (headers, body) = received.recv().await.unwrap();
        assert_eq!(headers[turn_state::HEADER_NAME], fresh);
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value.get("previous_response_id").and_then(|v| v.as_str()), Some("resp_1"));
        assert!(value.get("client_metadata").is_none());
        assert_eq!(app.logs.lock().await.back().unwrap().turn_state_action, "header_only_after_wait");

        app.turn_state.lock().await.invalidate_all();
        app.settings.lock().await.token_fetch_paused = true;
        let response = tokio::time::timeout(Duration::from_secs(1), proxy_http(app.clone(), request(true, true)))
            .await
            .expect("paused fetch must stop waiting immediately");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(String::from_utf8_lossy(&axum::body::to_bytes(response.into_body(), 1024).await.unwrap()).contains("已暂停获取 Token"));
        assert_eq!(app.logs.lock().await.back().unwrap().error_kind.as_deref(), Some("state_wait_fetch_paused"));
        assert!(received.try_recv().is_err());
        app.settings.lock().await.token_fetch_paused = false;

        let pending = tokio::spawn(proxy_http(app.clone(), request(true, true)));
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!pending.is_finished());
        app.settings.lock().await.token_fetch_paused = true;
        app.fetch_change_notify.notify_waiters();
        assert_eq!(pending.await.unwrap().status(), StatusCode::CONFLICT);
        assert_eq!(app.logs.lock().await.back().unwrap().error_kind.as_deref(), Some("state_wait_fetch_paused"));
        assert!(received.try_recv().is_err());
        app.settings.lock().await.token_fetch_paused = false;

        // Changes while waiting must never send the original request on stale
        // credentials or silently fall back to client state.
        for change in ["account", "policy", "route"] {
            app.turn_state.lock().await.invalidate_all();
            app.settings.lock().await.state_miss_policy = Wait;
            write_login(home.path(), "account-a");
            let pending = tokio::spawn(proxy_http(app.clone(), request(true, true)));
            tokio::time::sleep(Duration::from_millis(80)).await;
            assert!(!pending.is_finished());
            match change {
                "account" => write_login(home.path(), "account-b"),
                "policy" => app.settings.lock().await.state_miss_policy = Strip,
                _ => app.settings.lock().await.upstream.push_str("/changed"),
            }
            assert_eq!(pending.await.unwrap().status(), StatusCode::CONFLICT);
            assert!(received.try_recv().is_err());
        }

        // Use a real TCP client to verify disconnect cancels the Axum handler,
        // rather than merely aborting a synthetic task in a unit test.
        write_login(home.path(), "account-a");
        app.settings.lock().await.state_miss_policy = Wait;
        app.turn_state.lock().await.invalidate_all();
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let (dropped_tx, mut dropped_rx) = tokio::sync::mpsc::unbounded_channel();
        struct HandlerGuard(tokio::sync::mpsc::UnboundedSender<()>);
        impl Drop for HandlerGuard { fn drop(&mut self) { let _ = self.0.send(()); } }
        let route_app = app.clone();
        let route = axum::Router::new().fallback(move |req: Request<Body>| {
            let app = route_app.clone();
            let entered = entered_tx.clone();
            let guard = HandlerGuard(dropped_tx.clone());
            async move {
                let _guard = guard;
                entered.send(()).unwrap();
                proxy_http(app, req).await
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = listener.local_addr().unwrap();
        let proxy = tokio::spawn(async move { axum::serve(listener, route).await.unwrap(); });
        let mut socket = tokio::net::TcpStream::connect(listen).await.unwrap();
        let body = r#"{"model":"policy-model"}"#;
        socket.write_all(format!("POST /responses HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nx-codex-turn-state: client-state\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        entered_rx.recv().await.unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(dropped_rx.try_recv().is_err());
        drop(socket);
        tokio::time::timeout(Duration::from_secs(2), dropped_rx.recv()).await.expect("client disconnect must drop the waiting handler").unwrap();
        app.turn_state.lock().await.capture("policy-model", &fresh, "test");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(received.try_recv().is_err());
        proxy.abort();
        server.abort();
    }).await.unwrap();
}
