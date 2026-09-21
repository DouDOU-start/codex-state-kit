use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::login::ChatGptCredentials;
use crate::logs::{self, NetworkLogDetails};
use crate::settings::{OutboundMode, Settings};
use crate::turn_state::{self, HEADER_NAME, MAX_FUTURE_SKEW_SECS};

fn responses_url(upstream: &str) -> String {
    format!("{}/responses", upstream.trim().trim_end_matches('/'))
}

/// 有可用 292 token 时的巡检间隔
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// 单发未命中目标长度后的重试间隔
pub const RETRY_INTERVAL: Duration = Duration::from_secs(6);
pub const ERROR_BACKOFF: Duration = Duration::from_secs(30);
/// A 403 can be tied to one model or one rotating exit rather than invalid login.
pub const FORBIDDEN_BACKOFF: Duration = Duration::from_secs(30);
pub const AUTH_BACKOFF: Duration = Duration::from_secs(300);
pub const CONNECT_ATTEMPTS: usize = 4;
pub const CONNECT_RETRY_INTERVAL: Duration = Duration::from_secs(6);

const CODEX_IDENTITY_VERSION: &str = "0.153.4";
const CODEX_ORIGINATOR: &str = "codex-tui";
const CODEX_USER_AGENT_SUFFIX: &str = " (Ubuntu 22.4.0; x86_64) xterm-256color";
const MAX_FETCH_TICKET_AGE_SECS: i64 = 35 * 60;

const SESSION_PLACEHOLDER_LC: &str = "{session}";
const SESSION_PLACEHOLDER_UC: &str = "{SESSION}";
const PROBE_RESPONSE_ID_LIMIT: usize = 256 * 1024;

#[derive(Debug)]
pub(crate) struct FetchedTicket {
    pub token: String,
    pub previous_response_id: Option<String>,
}

pub fn outbound_proxy_for_client(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("socks5://") {
        format!("socks5h://{rest}")
    } else {
        raw.to_string()
    }
}

pub fn has_session_placeholder(raw: &str) -> bool {
    raw.contains(SESSION_PLACEHOLDER_LC) || raw.contains(SESSION_PLACEHOLDER_UC)
}

pub fn replace_session_placeholder(raw: &str, session: &str) -> String {
    raw.replace(SESSION_PLACEHOLDER_LC, session)
        .replace(SESSION_PLACEHOLDER_UC, session)
}

pub fn generate_proxy_session() -> String {
    use rand::Rng;
    rand::rng()
        .sample_iter(rand::distr::Alphanumeric)
        .take(8)
        .map(char::from)
        .collect()
}

pub fn normalize_proxy_session(session: Option<&str>) -> Option<String> {
    session
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 32
                && value.chars().all(|ch| ch.is_ascii_alphanumeric())
        })
        .map(|value| value.to_string())
}

/// 打票时把 `{session}` 换成新的随机出口；没有占位符则原样返回。
pub fn resolve_probe_proxy(raw: &str) -> (String, Option<String>) {
    let raw = raw.trim();
    if !has_session_placeholder(raw) {
        return (raw.to_string(), None);
    }
    let session = generate_proxy_session();
    (
        replace_session_placeholder(raw, &session),
        Some(session),
    )
}

/// 业务发送使用已绑定的 session。模板含 `{session}` 但还没绑定时失败，避免误走未解析地址。
pub fn apply_bound_session(raw: &str, session: Option<&str>) -> Result<String> {
    let raw = raw.trim();
    if !has_session_placeholder(raw) {
        return Ok(raw.to_string());
    }
    let session = normalize_proxy_session(session).ok_or_else(|| {
        anyhow::anyhow!("代理 URL 含 {{session}}，但还没有绑定出口。请先打到稳定 292")
    })?;
    Ok(replace_session_placeholder(raw, &session))
}

/// 每次调用时随机化代理 URL 中的 session ID（`-sid-XXX`），
/// 使每次 fetch 请求分配到不同出口 IP，避免 sticky session 锁定在坏 IP 上。
pub fn randomize_proxy_session(raw: &str) -> String {
    use rand::Rng;
    let raw = raw.trim();
    // 匹配 -sid-XXXX 部分，替换为随机 8 字符
    if let Some(sid_start) = raw.find("-sid-") {
        let after_sid = &raw[sid_start + 5..]; // skip "-sid-"
        // 找到下一个 '-' 或 ':' 或 '@' 作为 session ID 结束
        let sid_end = after_sid
            .find(['-', ':', '@'])
            .unwrap_or(after_sid.len());
        let rng_id: String = rand::rng()
            .sample_iter(rand::distr::Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();
        format!(
            "{}-sid-{}{}",
            &raw[..sid_start],
            rng_id,
            &raw[sid_start + 5 + sid_end..]
        )
    } else if raw.contains("-region-") && !raw.contains("-sid-") {
        // 用户 URL 没有 -sid-，自动插入一个随机 session ID
        // 格式: ...-region-XX → ...-region-XX-sid-RANDOM
        // 在 region 段后面，':'（密码分隔符）或 '@'（用户结束）之前插入
        if let Some(region_start) = raw.find("-region-") {
            let after_region = &raw[region_start + 8..];
            // 跳过 region 值（到下一个 '-' 或 ':' 或 '@'）
            let region_end = after_region
                .find([':', '@'])
                .unwrap_or(after_region.len());
            let rng_id: String = rand::rng()
                .sample_iter(rand::distr::Alphanumeric)
                .take(8)
                .map(char::from)
                .collect();
            format!(
                "{}-sid-{}{}",
                &raw[..region_start + 8 + region_end],
                rng_id,
                &raw[region_start + 8 + region_end..]
            )
        } else {
            raw.to_string()
        }
    } else {
        raw.to_string()
    }
}

pub fn proxy_auth_hint(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    if url.username().is_empty() {
        return None;
    }
    if url.password().is_none() {
        return Some(
            "代理地址里没有密码。请用 socks5://用户名:密码@主机:端口（用户名和密码之间是英文冒号）"
                .into(),
        );
    }
    None
}

pub fn http_client(outbound_proxy: &str) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .pool_max_idle_per_host(0);
    let proxy = outbound_proxy_for_client(outbound_proxy);
    if !proxy.is_empty() {
        builder = builder.proxy(reqwest::Proxy::all(&proxy).context("出站代理")?);
    }
    builder.build().context("build turn-state fetch client")
}

pub fn preferred_model(home: &Path) -> String {
    std::fs::read_to_string(home.join("config.toml"))
        .ok()
        .and_then(|raw| raw.parse::<toml::Value>().ok())
        .and_then(|value| {
            value
                .get("model")
                .and_then(toml::Value::as_str)
                .map(str::trim)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "gpt-6-astra".into())
}

pub fn probe_body(model: &str) -> serde_json::Value {
    json!({
        "model": model,
        "store": false,
        "stream": true,
        "instructions": "Reply with exactly: pong",
        "input": [{
            "role": "user",
            "content": [{"type": "input_text", "text": "ping"}]
        }]
    })
}

fn codex_user_agent() -> String {
    format!("{CODEX_ORIGINATOR}/{CODEX_IDENTITY_VERSION}{CODEX_USER_AGENT_SUFFIX}")
}

pub(crate) async fn fetch_turn_state_with_log(
    client: &reqwest::Client,
    settings: &Settings,
    creds: &ChatGptCredentials,
    model: &str,
    target_len: usize,
    allow_auto_quality: bool,
    details: &mut NetworkLogDetails,
) -> Result<FetchedTicket> {
    let url = responses_url(&settings.upstream);
    let effective_proxy = outbound_proxy_for_client(&settings.outbound_proxy);
    *details = logs::token_network_details(
        &settings.upstream,
        &effective_proxy,
        settings.outbound_mode == OutboundMode::Warp,
        model,
    );
    let probe = probe_body(model);
    details.account_id = Some(logs::safe_text(&creds.account_id, 128));
    details.account_email = creds.email.as_deref().map(|email| logs::safe_text(email, 254));
    details.body_bytes = serde_json::to_vec(&probe)
        .map(|body| body.len())
        .unwrap_or(0);
    let request_started = Instant::now();
    let response = match client
        .post(&url)
        .header("Authorization", format!("Bearer {}", creds.access_token))
        .header("ChatGPT-Account-ID", &creds.account_id)
        .header("Content-Type", "application/json")
        .header("Accept", "text/event-stream")
        .header("OpenAI-Beta", "responses=experimental")
        .header("Connection", "close")
        .header("session_id", Uuid::new_v4().to_string())
        .header("originator", CODEX_ORIGINATOR)
        .header("version", CODEX_IDENTITY_VERSION)
        .header("User-Agent", codex_user_agent())
        .json(&probe)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            details.response_header_ms = Some(request_started.elapsed().as_millis());
            details.error_kind = Some(logs::request_error_kind(&err));
            let hint = proxy_auth_hint(&settings.outbound_proxy)
                .unwrap_or_else(|| "出站代理连不上，或上游拒绝了这次探测请求".into());
            return Err(anyhow::anyhow!("{hint}: {err}"));
        }
    };
    details.response_header_ms = Some(request_started.elapsed().as_millis());
    details.response_status = Some(response.status().as_u16());
    details.peer_addr = response.remote_addr().map(|addr| addr.to_string());
    details.final_origin = Some(logs::endpoint_origin(response.url().as_str()));
    details.http_version = Some(
        match response.version() {
            reqwest::Version::HTTP_09 => "HTTP/0.9",
            reqwest::Version::HTTP_10 => "HTTP/1.0",
            reqwest::Version::HTTP_11 => "HTTP/1.1",
            reqwest::Version::HTTP_2 => "HTTP/2",
            reqwest::Version::HTTP_3 => "HTTP/3",
            _ => "HTTP/unknown",
        }
        .into(),
    );
    let status = response.status();
    let token = response
        .headers()
        .get(HEADER_NAME)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    details.returned_turn_state_len = token.as_ref().map(|value| value.len());
    if status != reqwest::StatusCode::OK {
        details.turn_state_action = "rejected_status".into();
        bail!("上游拒绝 turn-state 探测请求 ({status})");
    }
    let Some(token) = token else {
        details.turn_state_action = "missing".into();
        bail!("上游未返回 {HEADER_NAME} ({status})");
    };
    if turn_state::is_degraded_token(&token) {
        details.turn_state_action = "rejected_degraded".into();
        bail!("上游返回降级 turn-state 长度 {}", token.len());
    }
    let auto_quality_match = allow_auto_quality
        && matches!(
            token.len(),
            turn_state::QUALITY_TOKEN_LEN | turn_state::QUALITY_TOKEN_LEN_332
        );
    if token.len() != target_len && !auto_quality_match {
        details.turn_state_action = "rejected_length".into();
        bail!("上游返回非目标 turn-state 长度 {}", token.len());
    }
    if !token.starts_with("gAAAAA") {
        details.turn_state_action = "rejected_invalid".into();
        bail!("上游返回的 {HEADER_NAME} 无法解析");
    }
    let Some(parsed) = turn_state::TurnState::from_token(&token, "fetch") else {
        details.turn_state_action = "rejected_invalid".into();
        bail!("上游返回的 {HEADER_NAME} 无法解析");
    };
    let age = chrono::Utc::now().timestamp().saturating_sub(parsed.issued_unix);
    if age < -MAX_FUTURE_SKEW_SECS {
        details.turn_state_action = "rejected_future".into();
        bail!("上游返回的 {HEADER_NAME} 时间戳超前");
    }
    if age > MAX_FETCH_TICKET_AGE_SECS {
        details.turn_state_action = "rejected_stale".into();
        bail!("上游返回的 {HEADER_NAME} 已超过预取年龄");
    }
    details.turn_state_action = "received".into();
    let previous_response_id = read_probe_response_id(response).await;
    Ok(FetchedTicket {
        token,
        previous_response_id,
    })
}

fn is_response_id(id: &str) -> bool {
    let id = id.trim();
    (id.starts_with("resp_") || id.starts_with("response_"))
        && !id.is_empty()
        && id.len() <= 128
        && id.chars().all(|ch| ch.is_ascii_graphic())
}

fn response_id_from_value(value: &serde_json::Value) -> Option<String> {
    const KEYS: [&str; 2] = ["id", "response_id"];
    for key in KEYS {
        if let Some(id) = value.get(key).and_then(serde_json::Value::as_str).filter(|id| is_response_id(id)) {
            return Some(id.trim().to_string());
        }
    }
    if let Some(response) = value.get("response") {
        for key in KEYS {
            if let Some(id) = response
                .get(key)
                .and_then(serde_json::Value::as_str)
                .filter(|id| is_response_id(id))
            {
                return Some(id.trim().to_string());
            }
        }
    }
    None
}

pub(crate) fn parse_probe_response_id(body: &str) -> Option<String> {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(id) = response_id_from_value(&value) {
            return Some(id);
        }
    }
    for block in body.split("\n\n") {
        for line in block.lines() {
            let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line.trim());
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
                if let Some(id) = response_id_from_value(&value) {
                    return Some(id);
                }
            }
        }
    }
    None
}

async fn read_probe_response_id(response: reqwest::Response) -> Option<String> {
    use futures_util::StreamExt;
    let mut buf = String::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        if let Some(id) = parse_probe_response_id(&buf) {
            return Some(id);
        }
        if buf.len() > PROBE_RESPONSE_ID_LIMIT {
            break;
        }
    }
    parse_probe_response_id(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct MockState {
        status: StatusCode,
        token: Option<String>,
        body: Option<String>,
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        headers: HeaderMap,
        body: serde_json::Value,
    }

    fn token_for_len_at(target_len: usize, issued_unix: i64) -> String {
        let raw_len = target_len * 3 / 4;
        let mut raw = vec![0_u8; raw_len];
        raw[0] = 0x80;
        raw[1..9].copy_from_slice(&(issued_unix as u64).to_be_bytes());
        let token = URL_SAFE_NO_PAD.encode(raw);
        assert_eq!(token.len(), target_len);
        token
    }

    fn token_for_len(target_len: usize) -> String {
        token_for_len_at(target_len, chrono::Utc::now().timestamp())
    }

    async fn mock_response(
        State(state): State<MockState>,
        headers: HeaderMap,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        state
            .requests
            .lock()
            .expect("capture request")
            .push(CapturedRequest { headers, body });
        let mut headers = HeaderMap::new();
        if let Some(token) = &state.token {
            headers.insert(HEADER_NAME, HeaderValue::from_str(token).unwrap());
        }
        if let Some(body) = &state.body {
            return (state.status, headers, body.clone()).into_response();
        }
        (state.status, headers, Json(json!({ "ok": true }))).into_response()
    }

    async fn serve(
        status: StatusCode,
        token: Option<String>,
    ) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
        serve_with_body(status, token, None).await
    }

    async fn serve_with_body(
        status: StatusCode,
        token: Option<String>,
        body: Option<String>,
    ) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = MockState {
            status,
            token,
            body,
            requests: requests.clone(),
        };
        let app = Router::new()
            .route("/responses", post(mock_response))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), requests)
    }

    fn creds() -> ChatGptCredentials {
        ChatGptCredentials {
            email: Some("test@example.com".into()),
            access_token: "access".into(),
            account_id: "acct".into(),
            auth_mode: "chatgpt".into(),
            refreshable: true,
        }
    }

    #[test]
    fn probe_matches_sub2api_profile() {
        assert_eq!(
            probe_body("gpt-6-astra"),
            json!({
                "model": "gpt-6-astra",
                "store": false,
                "stream": true,
                "instructions": "Reply with exactly: pong",
                "input": [{
                    "role": "user",
                    "content": [{"type": "input_text", "text": "ping"}]
                }]
            })
        );
    }

    #[test]
    fn socks5_uses_remote_dns() {
        assert_eq!(
            outbound_proxy_for_client("socks5://user:pass@127.0.0.1:1080"),
            "socks5h://user:pass@127.0.0.1:1080"
        );
        assert!(proxy_auth_hint("socks5://onlyuser@127.0.0.1:1080").is_some());
        assert!(proxy_auth_hint("socks5://user:pass@127.0.0.1:1080").is_none());
    }

    #[test]
    fn randomize_proxy_session_rotates_sid() {
        let input = "socks5://xmtt1126849-region-SE-sid-FwdSE01-t-5:pass@us.arxlabs.io:3010";
        let a = randomize_proxy_session(input);
        let b = randomize_proxy_session(input);
        // session ID should be replaced, and two calls should differ
        assert_ne!(a, input);
        assert!(a.contains("-sid-"));
        assert!(a.contains("-t-5:pass@us.arxlabs.io:3010"));
        assert!(a.starts_with("socks5://xmtt1126849-region-SE-sid-"));
        assert_ne!(a, b, "两次随机化应该产生不同 session ID");
    }

    #[test]
    fn randomize_proxy_session_auto_inserts_sid() {
        // 没有 -sid- 的 URL 也要自动加上随机 session
        let input = "socks5://xmtt1126849-region-Rand:pass@us.arxlabs.io:3010";
        let a = randomize_proxy_session(input);
        let b = randomize_proxy_session(input);
        assert!(a.contains("-sid-"), "should insert -sid-: {a}");
        assert!(a.contains(":pass@"), "password should remain: {a}");
        assert_ne!(a, b, "两次随机化应该产生不同 session ID");
    }

    #[test]
    fn randomize_proxy_session_no_sid_passthrough() {
        let input = "socks5://user:pass@127.0.0.1:1080";
        assert_eq!(randomize_proxy_session(input), input);
    }

    #[test]
    fn session_placeholder_rotates_then_binds() {
        let template =
            "socks5://xmtt1126849-region-DE-sid-{session}-t-120:pass@us.arxlabs.io:3010";
        assert!(has_session_placeholder(template));
        let (probe_a, session_a) = resolve_probe_proxy(template);
        let (_probe_b, session_b) = resolve_probe_proxy(template);
        let session_a = session_a.expect("probe session");
        let session_b = session_b.expect("probe session");
        assert_ne!(session_a, session_b);
        assert!(probe_a.contains(&format!("-sid-{session_a}-t-120")));
        assert!(probe_a.contains(":pass@us.arxlabs.io:3010"));
        assert!(!probe_a.contains("{session}"));
        assert_eq!(
            apply_bound_session(template, Some(&session_a)).unwrap(),
            probe_a
        );
        assert!(apply_bound_session(template, None).unwrap_err().to_string().contains("{session}"));
        assert_eq!(
            apply_bound_session("socks5://127.0.0.1:1080", None).unwrap(),
            "socks5://127.0.0.1:1080"
        );
        assert_eq!(
            resolve_probe_proxy("socks5://127.0.0.1:1080"),
            ("socks5://127.0.0.1:1080".into(), None)
        );
    }

    #[test]
    fn reads_model_from_config() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("config.toml"), "model = \"gpt-6-astra\"\n").unwrap();
        assert_eq!(preferred_model(root.path()), "gpt-6-astra");
        assert_eq!(preferred_model(root.path().join("missing").as_path()), "gpt-6-astra");
    }

    #[tokio::test]
    async fn extracts_turn_state_header() {
        let expected = token_for_len(turn_state::QUALITY_TOKEN_LEN);
        let (upstream, requests) = serve(StatusCode::OK, Some(expected.clone())).await;
        let settings = Settings {
            upstream: upstream.clone(),
            ..Settings::default()
        };
        let mut details = NetworkLogDetails::default();
        let fetched = fetch_turn_state_with_log(
            &http_client("").unwrap(),
            &settings,
            &creds(),
            "gpt-6-astra",
            turn_state::QUALITY_TOKEN_LEN,
            false,
            &mut details,
        )
        .await
        .unwrap();
        assert_eq!(fetched.token, expected);
        assert_eq!(fetched.previous_response_id, None);
        assert_eq!(details.response_status, Some(200));
        assert!(details.response_header_ms.is_some());
        assert_eq!(details.turn_state_action, "received");
        assert_eq!(details.returned_turn_state_len, Some(fetched.token.len()));
        assert_eq!(details.peer_addr.as_deref(), upstream.strip_prefix("http://"));
        assert_eq!(details.final_origin.as_deref(), Some(upstream.as_str()));
        assert_eq!(details.http_version.as_deref(), Some("HTTP/1.1"));

        let captured = requests.lock().unwrap();
        let request = captured.first().unwrap();
        assert_eq!(request.body, probe_body("gpt-6-astra"));
        assert_eq!(request.headers["originator"], CODEX_ORIGINATOR);
        assert_eq!(request.headers["version"], CODEX_IDENTITY_VERSION);
        assert_eq!(request.headers["user-agent"], codex_user_agent());
        assert_eq!(request.headers["connection"], "close");
        assert_eq!(request.headers["openai-beta"], "responses=experimental");
        assert_eq!(request.headers["authorization"], "Bearer access");
        assert_eq!(request.headers["chatgpt-account-id"], "acct");
        assert!(request.headers.get(HEADER_NAME).is_none());
        Uuid::parse_str(request.headers["session_id"].to_str().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn missing_header_is_error() {
        let (upstream, _) = serve(StatusCode::OK, None).await;
        let settings = Settings {
            upstream,
            ..Settings::default()
        };
        let mut details = NetworkLogDetails::default();
        let err = fetch_turn_state_with_log(
            &http_client("").unwrap(),
            &settings,
            &creds(),
            "gpt-6-astra",
            turn_state::QUALITY_TOKEN_LEN,
            false,
            &mut details,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("未返回"));
        assert_eq!(details.response_status, Some(200));
        assert_eq!(details.turn_state_action, "missing");
        assert_eq!(details.returned_turn_state_len, None);
    }

    #[tokio::test]
    async fn each_probe_uses_a_fresh_session_id() {
        let token = token_for_len(turn_state::QUALITY_TOKEN_LEN);
        let (upstream, requests) = serve(StatusCode::OK, Some(token)).await;
        let settings = Settings {
            upstream,
            ..Settings::default()
        };
        for _ in 0..2 {
            let mut details = NetworkLogDetails::default();
            fetch_turn_state_with_log(
                &http_client("").unwrap(),
                &settings,
                &creds(),
                "gpt-6-astra",
                turn_state::QUALITY_TOKEN_LEN,
                false,
                &mut details,
            )
            .await
            .unwrap();
        }
        let captured = requests.lock().unwrap();
        let first = captured[0].headers["session_id"].to_str().unwrap();
        let second = captured[1].headers["session_id"].to_str().unwrap();
        assert_ne!(first, second);
        Uuid::parse_str(first).unwrap();
        Uuid::parse_str(second).unwrap();
    }

    #[tokio::test]
    async fn rejects_non_200_even_with_valid_ticket() {
        let token = token_for_len(turn_state::QUALITY_TOKEN_LEN);
        let (upstream, _) = serve(StatusCode::FORBIDDEN, Some(token)).await;
        let settings = Settings {
            upstream,
            ..Settings::default()
        };
        let mut details = NetworkLogDetails::default();
        let err = fetch_turn_state_with_log(
            &http_client("").unwrap(),
            &settings,
            &creds(),
            "gpt-6-astra",
            turn_state::QUALITY_TOKEN_LEN,
            false,
            &mut details,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("403"));
        assert_eq!(details.turn_state_action, "rejected_status");
    }

    #[tokio::test]
    async fn rejects_degraded_and_invalid_tickets() {
        for (token, action) in [
            (token_for_len(turn_state::DEGRADED_TOKEN_LEN), "rejected_degraded"),
            ("X".repeat(turn_state::QUALITY_TOKEN_LEN), "rejected_invalid"),
        ] {
            let (upstream, _) = serve(StatusCode::OK, Some(token)).await;
            let settings = Settings {
                upstream,
                ..Settings::default()
            };
            let mut details = NetworkLogDetails::default();
            fetch_turn_state_with_log(
                &http_client("").unwrap(),
                &settings,
                &creds(),
                "gpt-6-astra",
                turn_state::QUALITY_TOKEN_LEN,
                false,
                &mut details,
            )
            .await
            .unwrap_err();
            assert_eq!(details.turn_state_action, action);
        }
    }

    #[tokio::test]
    async fn rejects_wrong_exact_lengths_and_bad_timestamps() {
        let now = chrono::Utc::now().timestamp();
        let valid = token_for_len_at(turn_state::QUALITY_TOKEN_LEN, now);
        let cases = [
            (valid[..valid.len() - 1].to_string(), "rejected_length"),
            (format!("{valid}A"), "rejected_length"),
            (
                token_for_len_at(turn_state::QUALITY_TOKEN_LEN_332, now),
                "rejected_length",
            ),
            (
                token_for_len_at(
                    turn_state::QUALITY_TOKEN_LEN,
                    // Allow for wall-clock advancement during preceding HTTP
                    // cases; a one-second margin can become valid mid-test.
                    now + MAX_FUTURE_SKEW_SECS + 60,
                ),
                "rejected_future",
            ),
            (
                token_for_len_at(
                    turn_state::QUALITY_TOKEN_LEN,
                    now - MAX_FETCH_TICKET_AGE_SECS - 1,
                ),
                "rejected_stale",
            ),
            (token_for_len_at(turn_state::QUALITY_TOKEN_LEN, i64::MAX), "rejected_invalid"),
            (token_for_len_at(turn_state::QUALITY_TOKEN_LEN, i64::MIN), "rejected_invalid"),
        ];
        for (token, action) in cases {
            let (upstream, _) = serve(StatusCode::OK, Some(token)).await;
            let settings = Settings {
                upstream,
                ..Settings::default()
            };
            let mut details = NetworkLogDetails::default();
            fetch_turn_state_with_log(
                &http_client("").unwrap(),
                &settings,
                &creds(),
                "gpt-6-astra",
                turn_state::QUALITY_TOKEN_LEN,
                false,
                &mut details,
            )
            .await
            .unwrap_err();
            assert_eq!(details.turn_state_action, action);
        }

        let token_332 = token_for_len_at(turn_state::QUALITY_TOKEN_LEN_332, now);
        let (upstream, _) = serve(StatusCode::OK, Some(token_332.clone())).await;
        let settings = Settings {
            upstream,
            ..Settings::default()
        };
        let mut details = NetworkLogDetails::default();
        let accepted = fetch_turn_state_with_log(
            &http_client("").unwrap(),
            &settings,
            &creds(),
            "gpt-6-astra",
            turn_state::QUALITY_TOKEN_LEN_332,
            false,
            &mut details,
        )
        .await
        .unwrap();
        assert_eq!(accepted.token, token_332);

        let (upstream, _) = serve(StatusCode::OK, Some(token_332.clone())).await;
        let settings = Settings {
            upstream,
            ..Settings::default()
        };
        let mut details = NetworkLogDetails::default();
        let auto_accepted = fetch_turn_state_with_log(
            &http_client("").unwrap(),
            &settings,
            &creds(),
            "gpt-6-astra",
            turn_state::QUALITY_TOKEN_LEN,
            true,
            &mut details,
        )
        .await
        .unwrap();
        assert_eq!(auto_accepted.token, token_332);
    }

    #[test]
    fn parse_probe_response_id_reads_json_and_sse() {
        assert_eq!(
            parse_probe_response_id(r#"{"id":"resp_json"}"#).as_deref(),
            Some("resp_json")
        );
        assert_eq!(
            parse_probe_response_id(
                "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_sse\"}}\n\n"
            )
            .as_deref(),
            Some("resp_sse")
        );
        assert!(parse_probe_response_id(r#"{"ok":true}"#).is_none());
    }

    #[tokio::test]
    async fn extracts_probe_response_id_from_sse() {
        let expected = token_for_len(turn_state::QUALITY_TOKEN_LEN);
        let (upstream, _) = serve_with_body(
            StatusCode::OK,
            Some(expected.clone()),
            Some("event: response.created\ndata: {\"response\":{\"id\":\"resp_probe\"}}\n\n".into()),
        )
        .await;
        let settings = Settings {
            upstream,
            ..Settings::default()
        };
        let mut details = NetworkLogDetails::default();
        let fetched = fetch_turn_state_with_log(
            &http_client("").unwrap(),
            &settings,
            &creds(),
            "gpt-6-astra",
            turn_state::QUALITY_TOKEN_LEN,
            false,
            &mut details,
        )
        .await
        .unwrap();
        assert_eq!(fetched.token, expected);
        assert_eq!(fetched.previous_response_id.as_deref(), Some("resp_probe"));
    }
}
