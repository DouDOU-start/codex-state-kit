//! Outbound proxy URLs: the client-facing form, the `{session}` placeholder
//! for sticky-exit proxies, and the HTTP client used for latency probes.

use anyhow::{Context, Result};
use std::time::Duration;

const SESSION_PLACEHOLDER_LC: &str = "{session}";
const SESSION_PLACEHOLDER_UC: &str = "{SESSION}";

pub fn outbound_proxy_for_client(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("socks5://") {
        format!("socks5h://{rest}")
    } else {
        raw.to_string()
    }
}

/// The proxy URL to actually connect with: [`outbound_proxy_for_client`],
/// then routed through the system-proxy relay (see [`crate::system_proxy`]).
/// Use [`outbound_proxy_for_client`] for display and logs.
pub fn dial_proxy_for_client(raw: &str) -> String {
    crate::system_proxy::route(&outbound_proxy_for_client(raw))
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

fn normalize_proxy_session(session: Option<&str>) -> Option<String> {
    session
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 32
                && value.chars().all(|ch| ch.is_ascii_alphanumeric())
        })
        .map(|value| value.to_string())
}

/// 把 `{session}` 换成新的随机出口；没有占位符则原样返回。
pub fn resolve_session_proxy(raw: &str) -> (String, Option<String>) {
    let raw = raw.trim();
    if !has_session_placeholder(raw) {
        return (raw.to_string(), None);
    }
    let session = generate_proxy_session();
    (replace_session_placeholder(raw, &session), Some(session))
}

/// 用指定的 session 解析 `{session}`。模板含占位符但没有给出 session 时失败，避免误走未解析地址。
pub fn apply_bound_session(raw: &str, session: Option<&str>) -> Result<String> {
    let raw = raw.trim();
    if !has_session_placeholder(raw) {
        return Ok(raw.to_string());
    }
    let session = normalize_proxy_session(session)
        .ok_or_else(|| anyhow::anyhow!("代理 URL 含 {{session}}，但没有可用的出口 session"))?;
    Ok(replace_session_placeholder(raw, &session))
}

pub fn http_client(outbound_proxy: &str) -> Result<reqwest::Client> {
    // 与官方 CLI 一样走 TLS ALPN，由对端协商 HTTP/2。
    // 不用 http2_prior_knowledge：那是明文 h2c，HTTPS 和经 CONNECT 的隧道都会失败。
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::none())
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(4);
    let proxy = dial_proxy_for_client(outbound_proxy);
    if !proxy.is_empty() {
        builder = builder.proxy(reqwest::Proxy::all(&proxy).context("出站代理")?);
    }
    builder.build().context("build outbound client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_placeholder_rotates_then_binds() {
        let template = "socks5://xmtt1126849-region-DE-sid-{session}-t-120:pass@us.arxlabs.io:3010";
        assert!(has_session_placeholder(template));
        let (resolved_a, session_a) = resolve_session_proxy(template);
        let (_resolved_b, session_b) = resolve_session_proxy(template);
        let session_a = session_a.expect("session");
        let session_b = session_b.expect("session");
        assert_ne!(session_a, session_b);
        assert!(resolved_a.contains(&format!("-sid-{session_a}-t-120")));
        assert!(resolved_a.contains(":pass@us.arxlabs.io:3010"));
        assert!(!resolved_a.contains("{session}"));
        assert_eq!(
            apply_bound_session(template, Some(&session_a)).unwrap(),
            resolved_a
        );
        assert!(apply_bound_session(template, None)
            .unwrap_err()
            .to_string()
            .contains("{session}"));
        assert_eq!(
            apply_bound_session("socks5://127.0.0.1:1080", None).unwrap(),
            "socks5://127.0.0.1:1080"
        );
        assert_eq!(
            resolve_session_proxy("socks5://127.0.0.1:1080"),
            ("socks5://127.0.0.1:1080".into(), None)
        );
    }

    #[test]
    fn socks5_resolves_names_through_the_proxy() {
        assert_eq!(
            outbound_proxy_for_client(" socks5://127.0.0.1:1080 "),
            "socks5h://127.0.0.1:1080"
        );
        assert_eq!(
            outbound_proxy_for_client("http://127.0.0.1:7890"),
            "http://127.0.0.1:7890"
        );
    }
}
