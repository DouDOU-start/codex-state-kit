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

fn patch(settings: &Settings, proxy: &str) -> SettingsPatch {
    SettingsPatch {
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
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
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
    })
    .await
    .unwrap();
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
