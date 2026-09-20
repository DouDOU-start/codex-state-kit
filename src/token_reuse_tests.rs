use super::*;
use axum::Json;
use base64::Engine;
use serde_json::Value;

fn isolated_child(name: &str) -> bool {
    if std::env::var_os("CSK_REUSE_TEST_CHILD").is_some() {
        return true;
    }
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env("CSK_REUSE_TEST_CHILD", "1")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("APPDATA", home.path())
        .env("LOCALAPPDATA", home.path())
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
    false
}

fn write_login(home: &Path) {
    std::fs::write(login::kit_auth_path(home), serde_json::json!({
        "auth_mode":"chatgpt", "tokens":{
            "access_token":"reuse-test-access", "refresh_token":"reuse-test-refresh", "account_id":"reuse-account"
        }
    }).to_string()).unwrap();
}

fn ticket(age: i64) -> String {
    let mut bytes = vec![7; 219];
    bytes[0] = 0x80;
    bytes[1..9].copy_from_slice(&(chrono::Utc::now().timestamp() - age).to_be_bytes());
    base64::engine::general_purpose::URL_SAFE.encode(bytes)
}

fn request(model: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/responses")
        .header("content-type", "application/json")
        .header("chatgpt-account-id", "reuse-account")
        .header(turn_state::HEADER_NAME, "client-state")
        .body(Body::from(
            serde_json::json!({"model":model,"test_business":true}).to_string(),
        ))
        .unwrap()
}

fn patch(settings: &Settings, policy: TokenReusePolicy) -> SettingsPatch {
    SettingsPatch {
        proxy_listen: settings.proxy_listen.clone(),
        upstream: settings.upstream.clone(),
        codex_home: settings.codex_home.clone(),
        outbound_proxy: settings.outbound_proxy.clone(),
        upstream_proxy: settings.upstream_proxy.clone(),
        outbound_mode: settings.outbound_mode,
        warp_http2: settings.warp_http2,
        models: settings.models.clone(),
        state_miss_policy: settings.state_miss_policy,
        token_reuse_policy: policy,
    }
}

#[tokio::test]
async fn one_shared_probe_serves_all_models_until_prefetch() {
    if !isolated_child(
        "proxy::token_reuse_tests::one_shared_probe_serves_all_models_until_prefetch",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        let home = tempfile::tempdir().unwrap();
        write_login(home.path());
        let probes = Arc::new(AtomicU32::new(0));
        let probes_seen = probes.clone();
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let fresh = ticket(0);
        let reply_token = fresh.clone();
        let router =
            axum::Router::new().fallback(move |headers: HeaderMap, Json(body): Json<Value>| {
                let probes = probes_seen.clone();
                let sent = sent.clone();
                let token = reply_token.clone();
                async move {
                    if body["test_business"] == true {
                        sent.send((headers, body)).unwrap();
                        Response::new(Body::from("ok"))
                    } else {
                        probes.fetch_add(1, Ordering::Relaxed);
                        Response::builder()
                            .header(turn_state::HEADER_NAME, token)
                            .body(Body::empty())
                            .unwrap()
                    }
                }
            });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let app = Arc::new(
            App::new(Settings {
                upstream: endpoint.clone(),
                outbound_proxy: endpoint,
                outbound_mode: OutboundMode::Manual,
                codex_home: home.path().display().to_string(),
                models: vec!["a".into(), "b".into()],
                ..Settings::default()
            })
            .unwrap(),
        );
        assert_eq!(app.refresh_if_needed().await, fetch::CHECK_INTERVAL);
        assert_eq!(probes.load(Ordering::Relaxed), 1);
        let view = app.turn_state.lock().await.view();
        assert_eq!(view.status, "active");
        assert_eq!(view.models.len(), 2);
        assert!(view.shared_source_model.is_some());
        for n in 0..10 {
            let model = format!("new-{n}");
            app.turn_state.lock().await.register_model(&model);
            assert_eq!(app.refresh_if_needed().await, fetch::CHECK_INTERVAL);
            assert_eq!(
                probes.load(Ordering::Relaxed),
                1,
                "new models must not cause probes"
            );
            let response = proxy_http(app.clone(), request(&model)).await;
            assert_eq!(response.status(), StatusCode::OK);
            axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap();
            let (headers, body) = received.recv().await.unwrap();
            assert_eq!(headers[turn_state::HEADER_NAME], fresh);
            assert_eq!(
                body["model"], model,
                "sharing must not rewrite the requested model"
            );
        }
        // Two models may have queued refresh work at the same time; the fetch
        // gate recheck must use the already-fresh shared ticket without a probe.
        let (a, b) = tokio::join!(
            app.fetch_once_inner("a", true),
            app.fetch_once_inner("b", true)
        );
        assert_eq!(a.unwrap(), fresh);
        assert_eq!(b.unwrap(), fresh);
        assert_eq!(probes.load(Ordering::Relaxed), 1);

        // Explicit refresh also fetches only one donor, not every configured model.
        app.reset_fetch_schedule().await;
        app.refresh_turn_state().await.unwrap();
        assert_eq!(probes.load(Ordering::Relaxed), 2);
        // At 36 minutes, one new donor refreshes all twelve models together.
        {
            let mut store = app.turn_state.lock().await;
            store.invalidate_all();
            let aging = ticket(2160);
            store.capture("a", &aging, "fetch");
            assert_eq!(store.peek_for_model("b"), Some(aging));
        }
        app.reset_fetch_schedule().await;
        assert_eq!(app.refresh_if_needed().await, fetch::CHECK_INTERVAL);
        assert_eq!(probes.load(Ordering::Relaxed), 3);
        assert_eq!(app.refresh_if_needed().await, fetch::CHECK_INTERVAL);
        assert_eq!(probes.load(Ordering::Relaxed), 3);
        server.abort();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn policy_hot_switch_preserves_source_cache_and_survives_restart() {
    if !isolated_child(
        "proxy::token_reuse_tests::policy_hot_switch_preserves_source_cache_and_survives_restart",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        let home = tempfile::tempdir().unwrap();
        write_login(home.path());
        // No available WARP route: hot-switch tests must never probe real services.
        let settings = Settings {
            codex_home: home.path().display().to_string(),
            ..Settings::default()
        };
        let app = Arc::new(App::new(settings.clone()).unwrap());
        app.sync_logged_in_account().await;
        let token = ticket(0);
        {
            let mut store = app.turn_state.lock().await;
            store.register_model("source");
            store.register_model("consumer");
            store.capture("source", &token, "fetch");
            assert_eq!(
                store.peek_for_model("consumer").as_deref(),
                Some(token.as_str())
            );
        }
        let handle = ProxyHandle::new(app.clone());
        let cooldown = Instant::now() + Duration::from_secs(30);
        *app.fetch_next_allowed_at.lock().await = cooldown;
        let status = handle
            .apply_settings(patch(&settings, TokenReusePolicy::PerModel))
            .await
            .unwrap();
        handle.stop_fetch_loop().await;
        assert_eq!(*app.fetch_next_allowed_at.lock().await, cooldown);
        assert_eq!(status.token_reuse_policy, TokenReusePolicy::PerModel);
        assert_eq!(status.turn_state.status, "partial");
        assert!(app
            .turn_state
            .lock()
            .await
            .peek_for_model("consumer")
            .is_none());
        assert_eq!(
            app.turn_state
                .lock()
                .await
                .peek_for_model("source")
                .as_deref(),
            Some(token.as_str())
        );
        let loaded = crate::settings::load_settings();
        assert_eq!(loaded.token_reuse_policy, TokenReusePolicy::PerModel);
        let restarted = App::new(loaded).unwrap();
        assert!(restarted
            .turn_state
            .lock()
            .await
            .peek_for_model("consumer")
            .is_none());
        assert_eq!(
            restarted
                .turn_state
                .lock()
                .await
                .peek_for_model("source")
                .as_deref(),
            Some(token.as_str())
        );

        let status = handle
            .apply_settings(patch(&settings, TokenReusePolicy::Shared292))
            .await
            .unwrap();
        handle.stop_fetch_loop().await;
        assert_eq!(*app.fetch_next_allowed_at.lock().await, cooldown);
        assert_eq!(status.turn_state.status, "active");
        assert_eq!(status.token_reuse_policy, TokenReusePolicy::Shared292);
        let restarted = App::new(crate::settings::load_settings()).unwrap();
        assert_eq!(
            restarted
                .turn_state
                .lock()
                .await
                .peek_for_model("consumer")
                .as_deref(),
            Some(token.as_str())
        );
        // No tickets were copied into consumer: switching back is still isolated.
        restarted
            .turn_state
            .lock()
            .await
            .set_reuse_policy(TokenReusePolicy::PerModel);
        assert!(restarted
            .turn_state
            .lock()
            .await
            .peek_for_model("consumer")
            .is_none());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn late_degraded_shared_response_cannot_clear_new_ticket() {
    if !isolated_child(
        "proxy::token_reuse_tests::late_degraded_shared_response_cannot_clear_new_ticket",
    ) {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    write_login(home.path());
    let app = App::new(Settings {
        codex_home: home.path().display().to_string(),
        ..Settings::default()
    })
    .unwrap();
    let (creds, _) = app.sync_request_identity(home.path()).await.unwrap();
    let old = ticket(60);
    let fresh = ticket(0);
    app.turn_state.lock().await.capture("a", &old, "fetch");
    app.turn_state.lock().await.capture("b", &fresh, "fetch");
    assert!(
        !app.handle_degraded_response("consumer", &creds, true, Some(&old))
            .await
    );
    assert!(
        !app.handle_degraded_response("consumer", &creds, false, Some(&fresh))
            .await
    );
    assert_eq!(
        app.turn_state
            .lock()
            .await
            .peek_for_model("consumer")
            .as_deref(),
        Some(fresh.as_str())
    );
    assert!(
        app.handle_degraded_response("consumer", &creds, true, Some(&fresh))
            .await
    );
    for model in ["a", "b", "consumer"] {
        assert!(app.turn_state.lock().await.peek_for_model(model).is_none());
        assert!(app.turn_state.lock().await.needs_refresh(model));
    }
}

#[tokio::test]
async fn shared_ticket_unblocks_waiter_and_policy_change_cancels_wait() {
    if !isolated_child(
        "proxy::token_reuse_tests::shared_ticket_unblocks_waiter_and_policy_change_cancels_wait",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(8), async {
        let home = tempfile::tempdir().unwrap();
        write_login(home.path());
        let app = App::new(Settings {
            codex_home: home.path().display().to_string(),
            state_miss_policy: StateMissPolicy::Wait,
            ..Settings::default()
        })
        .unwrap();
        app.sync_logged_in_account().await;
        let settings = app.settings.lock().await.clone();
        let fresh = ticket(0);
        let produce = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            app.turn_state
                .lock()
                .await
                .capture("other-model", &fresh, "fetch");
        };
        let mut details = NetworkLogDetails::default();
        let (result, _) = tokio::join!(
            wait_for_request_state(
                &app,
                &settings,
                Some("reuse-account"),
                Some("consumer"),
                &mut details
            ),
            produce
        );
        assert_eq!(result.unwrap(), fresh);
        app.turn_state.lock().await.invalidate_all();
        let switch = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            app.settings.lock().await.token_reuse_policy = TokenReusePolicy::PerModel;
        };
        let (result, _) = tokio::join!(
            wait_for_request_state(
                &app,
                &settings,
                Some("reuse-account"),
                Some("consumer"),
                &mut details
            ),
            switch
        );
        assert!(result.is_err());
        assert_eq!(details.response_status, Some(409));
        assert_eq!(
            details.error_kind.as_deref(),
            Some("state_wait_policy_changed")
        );
    })
    .await
    .unwrap();
}
