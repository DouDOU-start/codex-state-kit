use anyhow::{bail, Context, Result};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::login::ChatGptCredentials;
use crate::logs::{self, NetworkLogDetails};
use crate::settings::{OutboundMode, Settings};
use crate::turn_state::{self, HEADER_NAME};

fn responses_url(upstream: &str) -> String {
    format!("{}/responses", upstream.trim().trim_end_matches('/'))
}

pub const REFRESH_AFTER_SECS: i64 = 1200;
/// 有可用 292 token 时的巡检间隔
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);
/// 并发打 292 全失败后的重试间隔
pub const RETRY_INTERVAL: Duration = Duration::from_secs(1);

pub fn outbound_proxy_for_client(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("socks5://") {
        format!("socks5h://{rest}")
    } else {
        raw.to_string()
    }
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
            .find(|c: char| c == '-' || c == ':' || c == '@')
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
                .find(|c: char| c == ':' || c == '@')
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
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(5));
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
        "input": [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "."}]
        }]
    })
}

pub(crate) async fn fetch_turn_state_with_log(
    client: &reqwest::Client,
    settings: &Settings,
    creds: &ChatGptCredentials,
    model: &str,
    details: &mut NetworkLogDetails,
) -> Result<String> {
    let url = responses_url(&settings.upstream);
    let effective_proxy = outbound_proxy_for_client(&settings.outbound_proxy);
    *details = logs::token_network_details(
        &settings.upstream,
        &effective_proxy,
        settings.outbound_mode == OutboundMode::Warp,
        model,
    );
    let probe = probe_body(model);
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
    if let Some(token) = turn_state::header_token(response.headers()) {
        details.turn_state_action = "received".into();
        details.returned_turn_state_len = Some(token.len());
        let _ = response.bytes().await;
        return Ok(token);
    }
    details.turn_state_action = "missing".into();
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let excerpt: String = body.chars().take(180).collect();
    bail!("上游未返回 {HEADER_NAME} ({status}) {excerpt}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderValue, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use tokio::net::TcpListener;

    async fn with_token() -> impl IntoResponse {
        let mut headers = HeaderMap::new();
        headers.insert(
            HEADER_NAME,
            HeaderValue::from_static("gAAAAAfetched-token"),
        );
        (headers, Json(json!({ "ok": true })))
    }

    async fn without_token() -> impl IntoResponse {
        (StatusCode::OK, Json(json!({ "ok": true })))
    }

    async fn serve(with_header: bool) -> String {
        let app = if with_header {
            Router::new().route("/responses", post(with_token))
        } else {
            Router::new().route("/responses", post(without_token))
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn creds() -> ChatGptCredentials {
        ChatGptCredentials {
            access_token: "access".into(),
            account_id: "acct".into(),
        }
    }

    #[test]
    fn probe_enables_stream() {
        assert_eq!(probe_body("gpt-6-astra")["stream"], json!(true));
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
        let input = "socks5://xmtt1126849-region-Rand:kfpbxiwv@us.arxlabs.io:3010";
        let a = randomize_proxy_session(input);
        let b = randomize_proxy_session(input);
        assert!(a.contains("-sid-"), "should insert -sid-: {a}");
        assert!(a.contains(":kfpbxiwv@"), "password should remain: {a}");
        assert_ne!(a, b, "两次随机化应该产生不同 session ID");
    }

    #[test]
    fn randomize_proxy_session_no_sid_passthrough() {
        let input = "socks5://user:pass@127.0.0.1:1080";
        assert_eq!(randomize_proxy_session(input), input);
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
        let upstream = serve(true).await;
        let settings = Settings {
            upstream: upstream.clone(),
            ..Settings::default()
        };
        let mut details = NetworkLogDetails::default();
        let token = fetch_turn_state_with_log(
            &http_client("").unwrap(),
            &settings,
            &creds(),
            "gpt-6-astra",
            &mut details,
        )
        .await
        .unwrap();
        assert_eq!(token, "gAAAAAfetched-token");
        assert_eq!(details.response_status, Some(200));
        assert!(details.response_header_ms.is_some());
        assert_eq!(details.turn_state_action, "received");
        assert_eq!(details.returned_turn_state_len, Some(token.len()));
        assert_eq!(details.peer_addr.as_deref(), upstream.strip_prefix("http://"));
        assert_eq!(details.final_origin.as_deref(), Some(upstream.as_str()));
    }

    #[tokio::test]
    async fn missing_header_is_error() {
        let upstream = serve(false).await;
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
            &mut details,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("未返回"));
        assert_eq!(details.response_status, Some(200));
        assert_eq!(details.turn_state_action, "missing");
        assert_eq!(details.returned_turn_state_len, None);
    }
}
