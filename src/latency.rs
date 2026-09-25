//! Connectivity checks against the configured GPT upstream.
//! Any HTTP response counts as reachable. The recorded delay is the time until
//! response headers arrive.
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::time::{Duration, Instant};

use crate::outbound;
use crate::settings::normalize_proxy;

pub const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
pub const NODE_PROBE_TARGET: &str = "https://www.gstatic.com/generate_204";

pub fn node_delay_url(controller: &str, node: &str, target: &str) -> Result<reqwest::Url> {
    let node = encode_path_segment(node);
    let mut url = reqwest::Url::parse(&format!("http://{controller}/proxies/{node}/delay"))?;
    url.query_pairs_mut()
        .append_pair("url", target)
        .append_pair("timeout", "4000")
        .append_pair("expected", "*");
    Ok(url)
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LatencySample {
    pub name: String,
    pub delay_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LatencyReport {
    pub target: String,
    pub samples: Vec<LatencySample>,
}

pub fn probe_target(upstream: &str) -> Result<String> {
    let upstream = upstream.trim();
    if upstream.is_empty() {
        bail!("未配置 GPT 上游地址");
    }
    let url = reqwest::Url::parse(upstream).context("GPT 上游地址无效")?;
    if url.scheme() != "http" && url.scheme() != "https" {
        bail!("GPT 上游地址无效");
    }
    if url.host_str().is_none() {
        bail!("GPT 上游地址无效");
    }
    Ok(url.to_string())
}

pub fn proxy_for_probe(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("请先填写代理地址");
    }
    let resolved = if outbound::has_session_placeholder(raw) {
        outbound::replace_session_placeholder(raw, "probe")
    } else {
        raw.to_string()
    };
    let normalized = normalize_proxy(&resolved, "代理")?;
    if normalized.is_empty() {
        bail!("请先填写代理地址");
    }
    Ok(outbound::outbound_proxy_for_client(&normalized))
}

pub(crate) fn encode_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

pub fn group_delay_url(controller: &str, group: &str, target: &str) -> Result<reqwest::Url> {
    let group = encode_path_segment(group);
    let mut url = reqwest::Url::parse(&format!("http://{controller}/group/{group}/delay"))
        .context("无法构造节点延迟检测地址")?;
    url.query_pairs_mut()
        .append_pair("url", target)
        .append_pair("timeout", &PROBE_TIMEOUT.as_millis().to_string())
        .append_pair("expected", "*");
    Ok(url)
}

pub fn samples_from_group_delays(nodes: &[String], body: &serde_json::Value) -> Vec<LatencySample> {
    let delays = body.as_object();
    nodes
        .iter()
        .map(|name| {
            let delay_ms = delays
                .and_then(|map| map.get(name))
                .and_then(serde_json::Value::as_u64)
                .filter(|delay| *delay > 0);
            LatencySample {
                name: name.clone(),
                delay_ms,
                error: delay_ms.is_none().then(|| "超时".into()),
            }
        })
        .collect()
}

pub fn sample_from_result(name: &str, result: Result<u64>) -> LatencySample {
    match result {
        Ok(delay_ms) => LatencySample {
            name: name.to_string(),
            delay_ms: Some(delay_ms),
            error: None,
        },
        Err(err) => LatencySample {
            name: name.to_string(),
            delay_ms: None,
            error: Some(err.to_string()),
        },
    }
}

pub async fn probe_through_proxy(proxy: &str, target: &str) -> Result<u64> {
    // Measure the same path business traffic takes (system-proxy relay).
    let proxy = crate::system_proxy::route(&proxy_for_probe(proxy)?);
    let client = crate::tls::http_client_builder()
        .proxy(reqwest::Proxy::all(&proxy).context("代理地址无效")?)
        .connect_timeout(PROBE_TIMEOUT)
        .timeout(PROBE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("无法创建检测客户端")?;
    let started = Instant::now();
    match client.get(target).send().await {
        Ok(response) => {
            let _ = response.status();
            Ok(started.elapsed().as_millis().max(1) as u64)
        }
        Err(err) if err.is_timeout() => bail!("超时"),
        Err(_) => bail!("无法连上 GPT 上游"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_probe_encodes_names_and_uses_short_timeout() {
        let url = node_delay_url("127.0.0.1:19090", "日本 / A?#", NODE_PROBE_TARGET).unwrap();
        assert_eq!(
            url.path(),
            "/proxies/%E6%97%A5%E6%9C%AC%20%2F%20A%3F%23/delay"
        );
        let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(query["timeout"], "4000");
        assert_eq!(query["url"], NODE_PROBE_TARGET);
        assert_eq!(query["expected"], "*");
        assert!(url.fragment().is_none());
    }

    #[test]
    fn probe_target_keeps_the_configured_upstream() {
        assert_eq!(
            probe_target("https://chatgpt.com/backend-api/codex").unwrap(),
            "https://chatgpt.com/backend-api/codex"
        );
        assert!(probe_target("").is_err());
        assert!(probe_target("socks5://127.0.0.1:1080").is_err());
    }

    #[test]
    fn proxy_for_probe_fills_session_and_uses_remote_dns() {
        let proxy = proxy_for_probe("socks5://user-sid-{session}:pass@host:3010").unwrap();
        assert_eq!(proxy, "socks5h://user-sid-probe:pass@host:3010");
        assert!(proxy_for_probe("").is_err());
    }

    #[test]
    fn group_delay_asks_mihomo_to_accept_any_http_status() {
        let url = group_delay_url(
            "127.0.0.1:19090",
            "Kit",
            "https://chatgpt.com/backend-api/codex",
        )
        .unwrap();
        let query = url.query().unwrap();
        assert!(query.contains("url=https%3A%2F%2Fchatgpt.com%2Fbackend-api%2Fcodex"));
        assert!(query.contains("timeout=8000"));
        assert!(query.contains("expected=%2A") || query.contains("expected=*"));
        assert_eq!(url.path(), "/group/Kit/delay");
    }

    #[test]
    fn missing_nodes_are_timeouts() {
        let body = serde_json::json!({"alpha": 186, "beta": 0});
        let samples =
            samples_from_group_delays(&["alpha".into(), "beta".into(), "gamma".into()], &body);
        assert_eq!(samples[0].delay_ms, Some(186));
        assert_eq!(samples[1].error.as_deref(), Some("超时"));
        assert_eq!(samples[2].error.as_deref(), Some("超时"));
    }
}
