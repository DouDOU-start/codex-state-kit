//! Loopback OAuth authorization-code login, following openai/codex's protocol.
use crate::login::{
    exchange_tokens, persist_tokens, LoginMethod, LoginStart, PollResult, PollStatus, CLIENT_ID,
};
use anyhow::{bail, Context, Result};
use axum::{
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use std::{
    net::{Ipv4Addr, TcpListener},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::watch, task::JoinHandle};

const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const LIFETIME: Duration = Duration::from_secs(900);

struct Progress {
    claimed: bool,
    result: Option<PollResult>,
}

struct CallbackState {
    state: String,
    verifier: String,
    redirect_uri: String,
    token_url: String,
    port: u16,
    home: PathBuf,
    client: reqwest::Client,
    progress: Mutex<Progress>,
    done: watch::Sender<bool>,
}

impl CallbackState {
    fn finish(&self, result: PollResult) {
        let mut progress = self.progress.lock().expect("callback progress");
        if progress.result.is_none() {
            progress.result = Some(result);
            self.done.send_replace(true);
        }
    }
}

pub struct BrowserLogin {
    inner: Arc<CallbackState>,
    task: JoinHandle<()>,
}

impl BrowserLogin {
    pub fn start(home: PathBuf) -> Result<(LoginStart, Self)> {
        // Both ports are registered by Codex. Never terminate a different app's listener.
        let listener = bind_ports(&[1455, 1457])?;
        Self::with_listener(home, listener, AUTHORIZE_URL, TOKEN_URL, LIFETIME)
    }

    fn with_listener(
        home: PathBuf,
        listener: TcpListener,
        authorize_url: &str,
        token_url: &str,
        lifetime: Duration,
    ) -> Result<(LoginStart, Self)> {
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let listener = tokio::net::TcpListener::from_std(listener)?;
        let verifier = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let state = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
        let redirect_uri = format!("http://localhost:{port}/auth/callback");
        let mut url = url::Url::parse(authorize_url)?;
        url.query_pairs_mut().extend_pairs([
            ("response_type", "code"),
            ("client_id", CLIENT_ID),
            ("redirect_uri", redirect_uri.as_str()),
            ("scope", "openid profile email offline_access"),
            (
                "code_challenge",
                URL_SAFE_NO_PAD
                    .encode(Sha256::digest(verifier.as_bytes()))
                    .as_str(),
            ),
            ("code_challenge_method", "S256"),
            ("state", state.as_str()),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
        ]);
        let start = LoginStart {
            method: LoginMethod::Browser,
            user_code: String::new(),
            verification_uri: url.to_string(),
            expires_in: lifetime.as_secs(),
            interval: 1,
        };
        let (done, mut receiver) = watch::channel(false);
        let inner = Arc::new(CallbackState {
            state,
            verifier,
            redirect_uri,
            token_url: token_url.into(),
            port,
            home,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            progress: Mutex::new(Progress {
                claimed: false,
                result: None,
            }),
            done,
        });
        let router = Router::new()
            .route("/auth/callback", get(callback))
            .with_state(inner.clone());
        let expiry = inner.clone();
        let failed = inner.clone();
        let task = tokio::spawn(async move {
            let result = axum::serve(listener, router).with_graceful_shutdown(async move {
                tokio::select! {
                    _ = receiver.wait_for(|value| *value) => {},
                    _ = tokio::time::sleep(lifetime) => expiry.finish(outcome(PollStatus::Expired, "登录已超时，请重新登录")),
                }
            }).await;
            if result.is_err() {
                failed.finish(outcome(PollStatus::Failed, "登录回调服务已停止，请重试"));
            }
        });
        Ok((start, Self { inner, task }))
    }

    pub fn poll(&self) -> PollResult {
        self.inner
            .progress
            .lock()
            .expect("callback progress")
            .result
            .clone()
            .unwrap_or(PollResult {
                status: PollStatus::Pending,
                message: None,
                login: None,
            })
    }

    pub fn cancel(&self) {
        self.inner.finish(outcome(PollStatus::Denied, "登录已取消"));
        self.task.abort();
    }
}

impl Drop for BrowserLogin {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn bind_ports(ports: &[u16]) -> Result<TcpListener> {
    for port in ports {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, *port)) {
            Ok(listener) => return Ok(listener),
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(err) => return Err(err).context("无法启动本机登录回调服务"),
        }
    }
    bail!("回调端口 1455 和 1457 均被占用，请关闭其他登录窗口或使用授权码登录")
}

fn outcome(status: PollStatus, message: &str) -> PollResult {
    PollResult {
        status,
        message: Some(message.into()),
        login: None,
    }
}

fn page(status: StatusCode, message: &str) -> Response {
    // All messages are local constants. Never echo OAuth query values or credentials.
    (status, [("Cache-Control", "no-store"), ("Referrer-Policy", "no-referrer"),
        ("Content-Security-Policy", "default-src 'none'; frame-ancestors 'none'"),
        ("X-Content-Type-Options", "nosniff")],
        Html(format!("<!doctype html><html lang=\"zh-CN\"><meta charset=\"utf-8\"><title>Codex State Kit</title><h1>Codex State Kit</h1><p>{message}</p></html>"))).into_response()
}

async fn callback(
    State(inner): State<Arc<CallbackState>>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> Response {
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if host != format!("localhost:{}", inner.port) && host != format!("127.0.0.1:{}", inner.port) {
        return page(StatusCode::BAD_REQUEST, "无效的回调地址。");
    }
    let pairs: Vec<_> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).collect();
    let values = |name: &str| {
        pairs
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_ref())
            .collect::<Vec<_>>()
    };
    let states = values("state");
    if states.len() != 1 || states[0] != inner.state {
        return page(
            StatusCode::BAD_REQUEST,
            "登录校验失败，请返回应用重新打开登录页面。",
        );
    }
    let codes = values("code");
    let errors = values("error");
    if codes.len() > 1 || errors.len() > 1 || (!codes.is_empty() && !errors.is_empty()) {
        return page(StatusCode::BAD_REQUEST, "无效的授权响应。");
    }
    {
        let mut progress = inner.progress.lock().expect("callback progress");
        if progress.claimed || progress.result.is_some() {
            return page(StatusCode::CONFLICT, "此登录请求已处理或取消，请返回应用。");
        }
        if errors.is_empty() && (codes.is_empty() || codes[0].is_empty()) {
            return page(StatusCode::BAD_REQUEST, "授权响应缺少必要参数。");
        }
        progress.claimed = true;
    }
    if !errors.is_empty() {
        inner.finish(outcome(
            PollStatus::Denied,
            "浏览器授权未完成，请重试或使用授权码登录",
        ));
        return page(StatusCode::OK, "授权未完成，请返回应用重试。");
    }
    let mut cancelled = inner.done.subscribe();
    let tokens = tokio::select! {
        biased;
        _ = cancelled.wait_for(|value| *value) => return page(StatusCode::GONE, "登录已取消或超时。"),
        result = exchange_tokens(&inner.client, &inner.token_url, codes[0], &inner.verifier, &inner.redirect_uri) => result,
    };
    // The same mutex gates cancel/timeout and the credential write. Late callbacks cannot overwrite auth.
    let mut progress = inner.progress.lock().expect("callback progress");
    if progress.result.is_some() {
        return page(StatusCode::GONE, "登录已取消或超时。");
    }
    let result = tokens.and_then(|tokens| persist_tokens(&inner.home, &tokens));
    let response = match result {
        Ok(login) => {
            progress.result = Some(PollResult {
                status: PollStatus::Ok,
                message: Some("已登录 ChatGPT".into()),
                login: Some(login),
            });
            page(
                StatusCode::OK,
                "登录成功，可以关闭此页面并返回 Codex State Kit。",
            )
        }
        Err(_) => {
            progress.result = Some(outcome(
                PollStatus::Failed,
                "授权交换或保存失败，请重新登录",
            ));
            page(StatusCode::BAD_GATEWAY, "登录未完成，请返回应用重试。")
        }
    };
    inner.done.send_replace(true);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::Form, routing::post, Json};
    use serde_json::{json, Value};
    use std::collections::HashMap;

    struct Fixture {
        home: tempfile::TempDir,
        start: LoginStart,
        login: BrowserLogin,
        forms: Arc<Mutex<Vec<HashMap<String, String>>>>,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
        server: JoinHandle<()>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
        }
    }

    impl Fixture {
        async fn new(valid: bool, delayed: bool, lifetime: Duration) -> Self {
            let forms = Arc::new(Mutex::new(Vec::new()));
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let captured = forms.clone();
            let notify = entered.clone();
            let resume = release.clone();
            let app = Router::new().route("/token", post(move |Form(form): Form<HashMap<String, String>>| {
                let captured = captured.clone();
                let notify = notify.clone();
                let resume = resume.clone();
                async move {
                    captured.lock().unwrap().push(form);
                    notify.notify_one();
                    if delayed { resume.notified().await; }
                    let payload = URL_SAFE_NO_PAD.encode(json!({"email":"test@example.com", "chatgpt_account_id":"account-test"}).to_string());
                    Json(json!({"access_token":"test-access", "refresh_token":"test-refresh",
                        "id_token": if valid { Some(format!("e30.{payload}.sig")) } else { None }}))
                }
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let token_url = format!("http://{}/token", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let home = tempfile::tempdir().unwrap();
            let (start, login) = BrowserLogin::with_listener(
                home.path().into(),
                TcpListener::bind("127.0.0.1:0").unwrap(),
                AUTHORIZE_URL,
                &token_url,
                lifetime,
            )
            .unwrap();
            Self {
                home,
                start,
                login,
                forms,
                entered,
                release,
                server,
            }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}/auth/callback", self.login.inner.port)
        }

        fn request(&self) -> reqwest::RequestBuilder {
            reqwest::Client::new().get(self.url()).query(&[
                ("state", self.login.inner.state.as_str()),
                ("code", "test-code"),
            ])
        }
    }

    #[tokio::test]
    async fn callback_validates_state_and_pkce_and_writes_native_auth() {
        let f = Fixture::new(true, false, LIFETIME).await;
        for query in ["code=secret", "state=wrong&code=secret"] {
            let response = reqwest::get(format!("{}?{query}", f.url())).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            assert!(!response.text().await.unwrap().contains("secret"));
        }
        assert!(f.forms.lock().unwrap().is_empty());
        assert_eq!(f.login.poll().status, PollStatus::Pending);
        let response = f.request().send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.text().await.unwrap().contains("test-access"));
        let auth: Value = serde_json::from_slice(
            &std::fs::read(crate::login::kit_auth_path(f.home.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(f.home.path().join("auth.json")).unwrap(),
            std::fs::read(crate::login::kit_auth_path(f.home.path())).unwrap()
        );
        assert_eq!(auth["auth_mode"], "chatgpt");
        assert_eq!(auth["tokens"]["account_id"], "account-test");
        let url = url::Url::parse(&f.start.verification_uri).unwrap();
        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        let forms = f.forms.lock().unwrap();
        let form = &forms[0];
        assert_eq!(forms.len(), 1);
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["client_id"], CLIENT_ID);
        assert_eq!(form["code"], "test-code");
        assert_eq!(form["redirect_uri"], params["redirect_uri"]);
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(
            params["code_challenge"],
            URL_SAFE_NO_PAD.encode(Sha256::digest(form["code_verifier"].as_bytes()))
        );
        assert_eq!(f.login.poll().status, PollStatus::Ok);
        let status = serde_json::to_string(&f.login.poll().login).unwrap();
        assert!(!status.contains("test-access"));
        assert!(!status.contains("test-refresh"));
    }

    #[tokio::test]
    async fn malformed_callback_does_not_consume_session() {
        let f = Fixture::new(true, false, LIFETIME).await;
        for suffix in [
            "",
            "&code=a&code=b",
            "&code=a&error=denied",
            "&state=duplicate&code=a",
        ] {
            let response =
                reqwest::get(format!("{}?state={}{suffix}", f.url(), f.login.inner.state))
                    .await
                    .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let response = f
            .request()
            .header("Host", "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(f.forms.lock().unwrap().is_empty());
        assert_eq!(f.request().send().await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cancellation_blocks_in_flight_credentials_and_replay() {
        let f = Fixture::new(true, true, LIFETIME).await;
        let path = f.home.path().join("auth.json");
        std::fs::write(&path, "original-auth").unwrap();
        let request = f.request();
        let pending = tokio::spawn(async move { request.send().await });
        tokio::time::timeout(Duration::from_secs(5), f.entered.notified())
            .await
            .unwrap();
        assert_eq!(
            f.request().send().await.unwrap().status(),
            StatusCode::CONFLICT
        );
        f.login.cancel();
        f.release.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), "original-auth");
        assert_eq!(f.forms.lock().unwrap().len(), 1);
        assert_eq!(f.login.poll().status, PollStatus::Denied);
        tokio::time::timeout(Duration::from_secs(5), async {
            while tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, f.login.inner.port))
                .await
                .is_ok()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn denied_and_invalid_tokens_preserve_existing_auth() {
        for denied in [true, false] {
            let f = Fixture::new(false, false, LIFETIME).await;
            let path = f.home.path().join("auth.json");
            std::fs::write(&path, "original-auth").unwrap();
            let response = if denied {
                reqwest::Client::new()
                    .get(f.url())
                    .query(&[
                        ("state", f.login.inner.state.as_str()),
                        ("error", "access_denied"),
                    ])
                    .send()
                    .await
                    .unwrap()
            } else {
                f.request().send().await.unwrap()
            };
            assert_eq!(
                response.status(),
                if denied {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_GATEWAY
                }
            );
            assert_eq!(
                f.login.poll().status,
                if denied {
                    PollStatus::Denied
                } else {
                    PollStatus::Failed
                }
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), "original-auth");
        }
    }

    #[tokio::test]
    async fn expiry_releases_listener_without_writing_auth() {
        let f = Fixture::new(true, false, Duration::from_millis(20)).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while f.login.poll().status == PollStatus::Pending {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(f.login.poll().status, PollStatus::Expired);
        assert!(!f.home.path().join("auth.json").exists());
        tokio::time::timeout(Duration::from_secs(5), async {
            while !f.login.task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, f.login.inner.port))
                .await
                .is_err()
        );
    }

    #[test]
    fn busy_ports_report_error_and_allow_fallback() {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let second = TcpListener::bind("127.0.0.1:0").unwrap();
        let ports = [
            first.local_addr().unwrap().port(),
            second.local_addr().unwrap().port(),
        ];
        assert!(bind_ports(&ports).is_err());
        drop(second);
        assert_eq!(
            bind_ports(&ports).unwrap().local_addr().unwrap().port(),
            ports[1]
        );
    }
}
