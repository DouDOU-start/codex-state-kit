use super::*;
use base64::Engine;
use serde_json::Value;

fn decode_json_body(headers: &HeaderMap, body: &[u8]) -> Value {
    let plain = match headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
    {
        Some(encoding) if encoding.eq_ignore_ascii_case("zstd") => {
            zstd::decode_all(body).unwrap_or_else(|_| body.to_vec())
        }
        _ => body.to_vec(),
    };
    serde_json::from_slice(&plain).unwrap_or(Value::Null)
}

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

fn completed_probe_body(body: &Value) -> Body {
    let model = body.get("model").and_then(Value::as_str).unwrap_or("");
    Body::from(format!(
        "data: {{\"type\":\"response.completed\",\"response\":{{\"model\":\"{model}\"}}}}\n\n"
    ))
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
        state_fetch_model: settings.state_fetch_model.clone(),
        network_route_policy: settings.network_route_policy,
        forced_model: settings.forced_model.clone(),
        token_fetch_paused: settings.token_fetch_paused,
        token_max_age_mins: settings.token_max_age_mins,
        token_prefetch_age_mins: settings.token_prefetch_age_mins,
        mihomo_subscription: String::new(),
        mihomo_node: String::new(),
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
            axum::Router::new().fallback(move |headers: HeaderMap, raw: axum::body::Bytes| {
                let probes = probes_seen.clone();
                let sent = sent.clone();
                let token = reply_token.clone();
                async move {
                    let body = decode_json_body(&headers, &raw);
                    if body["test_business"] == true {
                        sent.send((headers, body)).unwrap();
                        Response::new(Body::from("ok"))
                    } else {
                        probes.fetch_add(1, Ordering::Relaxed);
                        Response::builder()
                            .header(turn_state::HEADER_NAME, token)
                            .body(completed_probe_body(&body))
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
        // 满 30 秒后仍可注入，但会再采一张共享票。
        {
            let mut store = app.turn_state.lock().await;
            store.invalidate_all();
            let aging = ticket(200);
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
async fn manual_refresh_bypasses_pause_and_cooldown() {
    if !isolated_child("proxy::token_reuse_tests::manual_refresh_bypasses_pause_and_cooldown") {
        return;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        let home = tempfile::tempdir().unwrap();
        write_login(home.path());
        let probes = Arc::new(AtomicU32::new(0));
        let probes_seen = probes.clone();
        let fresh = ticket(0);
        let reply_token = fresh.clone();
        let router =
            axum::Router::new().fallback(move |headers: HeaderMap, raw: axum::body::Bytes| {
                let probes = probes_seen.clone();
                let token = reply_token.clone();
                async move {
                    let body = decode_json_body(&headers, &raw);
                    if body["test_business"] == true {
                        Response::new(Body::from("ok"))
                    } else {
                        probes.fetch_add(1, Ordering::Relaxed);
                        Response::builder()
                            .header(turn_state::HEADER_NAME, token)
                            .body(completed_probe_body(&body))
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
                models: vec!["a".into()],
                ..Settings::default()
            })
            .unwrap(),
        );
        assert_eq!(app.refresh_if_needed().await, fetch::CHECK_INTERVAL);
        assert_eq!(probes.load(Ordering::Relaxed), 1);

        app.settings.lock().await.token_fetch_paused = true;
        app.defer_next_fetch(Duration::from_secs(600)).await;
        app.fetch_model_next_allowed_at
            .lock()
            .await
            .insert("a".into(), Instant::now() + Duration::from_secs(600));

        assert_eq!(app.refresh_if_needed().await, fetch::CHECK_INTERVAL);
        assert_eq!(
            probes.load(Ordering::Relaxed),
            1,
            "paused background loop must not probe"
        );
        let err = app.fetch_once("a").await.unwrap_err();
        assert_eq!(err.retry, FetchRetryClass::Deferred);

        let status = app.refresh_turn_state().await.unwrap();
        assert_eq!(probes.load(Ordering::Relaxed), 2);
        assert_eq!(status.turn_state.status, "active");
        assert_ne!(
            status.fetch_error.as_deref(),
            Some(TOKEN_MANUAL_REFRESH_MESSAGE)
        );
        assert!(app.settings.lock().await.token_fetch_paused);
        server.abort();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn pinned_donor_is_the_only_model_that_fetches_shared_292() {
    if !isolated_child(
        "proxy::token_reuse_tests::pinned_donor_is_the_only_model_that_fetches_shared_292",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        let home = tempfile::tempdir().unwrap();
        write_login(home.path());
        let probed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let probed_seen = probed.clone();
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let fresh = ticket(0);
        let reply_token = fresh.clone();
        let router =
            axum::Router::new().fallback(move |headers: HeaderMap, raw: axum::body::Bytes| {
                let probed = probed_seen.clone();
                let sent = sent.clone();
                let token = reply_token.clone();
                async move {
                    let body = decode_json_body(&headers, &raw);
                    if body["test_business"] == true {
                        sent.send(body).unwrap();
                        Response::new(Body::from("ok"))
                    } else {
                        probed
                            .lock()
                            .unwrap()
                            .push(body["model"].as_str().unwrap_or("").to_string());
                        Response::builder()
                            .header(turn_state::HEADER_NAME, token)
                            .body(completed_probe_body(&body))
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
                state_fetch_model: "gpt-5.5".into(),
                ..Settings::default()
            })
            .unwrap(),
        );
        assert_eq!(app.refresh_if_needed().await, fetch::CHECK_INTERVAL);
        assert_eq!(probed.lock().unwrap().as_slice(), ["gpt-5.5".to_string()]);
        app.refresh_turn_state().await.unwrap();
        assert_eq!(
            probed.lock().unwrap().as_slice(),
            ["gpt-5.5".to_string(), "gpt-5.5".to_string()]
        );
        let response = proxy_http(app.clone(), request("a")).await;
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let body = received.recv().await.unwrap();
        assert_eq!(body["model"], "a");
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
async fn degraded_business_response_invalidates_current_shared_ticket() {
    if !isolated_child(
        "proxy::token_reuse_tests::degraded_business_response_invalidates_current_shared_ticket",
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
    let fresh = ticket(0);
    app.turn_state.lock().await.capture("a", &fresh, "fetch");
    app.observe_business_response("a", Some(&fresh), true, None, false)
        .await;
    assert!(app.turn_state.lock().await.peek_for_model("a").is_none());
    assert!(app.turn_state.lock().await.needs_refresh("a"));
    assert!(app.degraded.load(Ordering::Relaxed));

    let echoed = ticket(40);
    app.turn_state.lock().await.capture("a", &echoed, "fetch");
    app.degraded.store(false, Ordering::Relaxed);
    app.observe_business_response("a", Some(&echoed), false, Some("a"), true)
        .await;
    assert_eq!(
        app.turn_state.lock().await.peek_for_model("a").as_deref(),
        Some(echoed.as_str())
    );
    assert!(app.turn_state.lock().await.needs_refresh("a"));
    assert!(!app.degraded.load(Ordering::Relaxed));

    let newer = ticket(1);
    app.turn_state.lock().await.capture("a", &newer, "fetch");
    app.observe_business_response("a", Some(&echoed), true, None, false)
        .await;
    assert_eq!(
        app.turn_state.lock().await.peek_for_model("a").as_deref(),
        Some(newer.as_str())
    );

    app.observe_business_response("a", Some(&newer), false, Some("other-model"), true)
        .await;
    assert!(app.turn_state.lock().await.peek_for_model("a").is_none());
    assert!(app.turn_state.lock().await.needs_refresh("a"));
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

#[tokio::test]
async fn miss_streak_doubles_probe_concurrency_then_resets() {
    if !isolated_child(
        "proxy::token_reuse_tests::miss_streak_doubles_probe_concurrency_then_resets",
    ) {
        return;
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        let home = tempfile::tempdir().unwrap();
        write_login(home.path());
        let probes = Arc::new(AtomicU32::new(0));
        let probes_seen = probes.clone();
        let fresh = ticket(0);
        let reply_token = fresh.clone();
        let router =
            axum::Router::new().fallback(move |headers: HeaderMap, raw: axum::body::Bytes| {
                let probes = probes_seen.clone();
                let token = reply_token.clone();
                async move {
                    let body = decode_json_body(&headers, &raw);
                    let n = probes.fetch_add(1, Ordering::Relaxed) + 1;
                    let value = if n <= 3 { "x".repeat(312) } else { token };
                    Response::builder()
                        .header(turn_state::HEADER_NAME, value)
                        .body(completed_probe_body(&body))
                        .unwrap()
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
                models: vec!["a".into()],
                ..Settings::default()
            })
            .unwrap(),
        );
        let _ = app.refresh_if_needed().await;
        assert_eq!(probes.load(Ordering::Relaxed), 1);
        app.reset_fetch_schedule().await;
        let _ = app.refresh_if_needed().await;
        assert_eq!(probes.load(Ordering::Relaxed), 3);
        app.reset_fetch_schedule().await;
        assert_eq!(app.refresh_if_needed().await, fetch::CHECK_INTERVAL);
        assert_eq!(probes.load(Ordering::Relaxed), 7);
        assert_eq!(
            app.turn_state.lock().await.peek_for_model("a").as_deref(),
            Some(fresh.as_str())
        );
        app.turn_state.lock().await.invalidate_all();
        app.reset_fetch_schedule().await;
        assert_eq!(app.refresh_if_needed().await, fetch::CHECK_INTERVAL);
        assert_eq!(
            probes.load(Ordering::Relaxed),
            8,
            "successful capture must reset burst to 1"
        );
        server.abort();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn max_burst_miss_rests_then_restarts_at_one() {
    if !isolated_child("proxy::token_reuse_tests::max_burst_miss_rests_then_restarts_at_one") {
        return;
    }
    tokio::time::timeout(Duration::from_secs(20), async {
        let home = tempfile::tempdir().unwrap();
        write_login(home.path());
        let probes = Arc::new(AtomicU32::new(0));
        let probes_seen = probes.clone();
        let router =
            axum::Router::new().fallback(move |headers: HeaderMap, raw: axum::body::Bytes| {
                let probes = probes_seen.clone();
                async move {
                    let body = decode_json_body(&headers, &raw);
                    probes.fetch_add(1, Ordering::Relaxed);
                    Response::builder()
                        .header(turn_state::HEADER_NAME, "x".repeat(312))
                        .body(completed_probe_body(&body))
                        .unwrap()
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
                models: vec!["a".into()],
                ..Settings::default()
            })
            .unwrap(),
        );
        let _ = app.refresh_if_needed().await;
        assert_eq!(probes.load(Ordering::Relaxed), 1);
        app.reset_fetch_schedule().await;
        let _ = app.refresh_if_needed().await;
        assert_eq!(probes.load(Ordering::Relaxed), 3);
        app.reset_fetch_schedule().await;
        let wait = app.refresh_if_needed().await;
        assert_eq!(probes.load(Ordering::Relaxed), 7);
        assert!(
            wait >= Duration::from_secs(50),
            "打满 4 路后应静置约 60 秒，实际 {wait:?}"
        );
        assert!(wait <= fetch::BURST_EXHAUSTED_BACKOFF);
        let message = app.fetch_error.lock().await.clone().unwrap_or_default();
        assert!(message.contains("静置 60 秒"), "{message}");
        app.reset_fetch_schedule().await;
        let _ = app.refresh_if_needed().await;
        assert_eq!(
            probes.load(Ordering::Relaxed),
            8,
            "静置结束后应从 1 路重来，而不是继续 4 路"
        );
        server.abort();
    })
    .await
    .unwrap();
}
