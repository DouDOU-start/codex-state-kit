use serde::Serialize;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use url::Url;

pub const ROUTE_DEFAULT_SYSTEM: &str = "default_system";
pub const ROUTE_EXPLICIT_PROXY: &str = "explicit_proxy";
pub const ROUTE_EMBEDDED_WARP: &str = "embedded_warp";
pub const ROUTE_MANUAL_PROXY: &str = "manual_proxy";
static LOG_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const MAX_LOGS: usize = 80;
const MAX_TOKEN_FETCH_LOGS: usize = 30;

#[derive(Clone, Debug, Default)]
pub struct NetworkLogDetails {
    pub flow: String,
    pub transport: String,
    pub target_origin: String,
    pub final_origin: Option<String>,
    pub route_kind: String,
    pub proxy_endpoint: Option<String>,
    pub peer_addr: Option<String>,
    pub http_version: Option<String>,
    pub model: Option<String>,
    pub content_encoding: String,
    pub body_bytes: usize,
    pub turn_state_action: String,
    pub turn_state_len: Option<usize>,
    pub returned_turn_state_len: Option<usize>,
    pub error_kind: Option<String>,
    pub response_status: Option<u16>,
    pub response_header_ms: Option<u128>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub id: u64,
    pub ts: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub ms: u128,
    pub flow: String,
    pub transport: String,
    pub target_origin: String,
    pub final_origin: Option<String>,
    pub route_kind: String,
    pub proxy_endpoint: Option<String>,
    pub peer_addr: Option<String>,
    pub http_version: Option<String>,
    pub model: Option<String>,
    pub content_encoding: String,
    pub body_bytes: usize,
    pub turn_state_action: String,
    pub turn_state_len: Option<usize>,
    pub returned_turn_state_len: Option<usize>,
    pub error_kind: Option<String>,
}

impl LogEntry {
    pub fn new(
        method: &str,
        path: &str,
        status: u16,
        started: Instant,
        details: NetworkLogDetails,
    ) -> Self {
        let ms = details
            .response_header_ms
            .unwrap_or_else(|| started.elapsed().as_millis());
        Self {
            id: LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            ts: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            method: method.to_string(),
            path: path.to_string(),
            status,
            ms,
            flow: details.flow,
            transport: details.transport,
            target_origin: details.target_origin,
            final_origin: details.final_origin,
            route_kind: details.route_kind,
            proxy_endpoint: details.proxy_endpoint,
            peer_addr: details.peer_addr,
            http_version: details.http_version,
            model: details.model,
            content_encoding: details.content_encoding,
            body_bytes: details.body_bytes,
            turn_state_action: details.turn_state_action,
            turn_state_len: details.turn_state_len,
            returned_turn_state_len: details.returned_turn_state_len,
            error_kind: details.error_kind,
        }
    }
}

pub fn safe_text(raw: &str, max_chars: usize) -> String {
    raw.trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect()
}

pub fn endpoint_origin(raw: &str) -> String {
    let Ok(url) = Url::parse(raw.trim()) else {
        return "invalid-endpoint".into();
    };
    let Some(host) = url.host_str() else {
        return "invalid-endpoint".into();
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match url.port_or_known_default() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    }
}

pub fn network_details(upstream: &str, proxy: &str) -> NetworkLogDetails {
    let proxy = proxy.trim();
    NetworkLogDetails {
        flow: "business".into(),
        transport: "http".into(),
        target_origin: endpoint_origin(upstream),
        route_kind: if proxy.is_empty() {
            ROUTE_DEFAULT_SYSTEM.into()
        } else {
            ROUTE_EXPLICIT_PROXY.into()
        },
        proxy_endpoint: (!proxy.is_empty()).then(|| endpoint_origin(proxy)),
        content_encoding: "none".into(),
        turn_state_action: "not_applicable".into(),
        ..NetworkLogDetails::default()
    }
}

pub fn token_network_details(
    upstream: &str,
    proxy: &str,
    embedded_warp: bool,
    model: &str,
) -> NetworkLogDetails {
    NetworkLogDetails {
        flow: "token_fetch".into(),
        transport: "http_sse".into(),
        target_origin: endpoint_origin(upstream),
        route_kind: if embedded_warp {
            ROUTE_EMBEDDED_WARP.into()
        } else {
            ROUTE_MANUAL_PROXY.into()
        },
        proxy_endpoint: Some(endpoint_origin(proxy)),
        model: Some(safe_text(model, 80)).filter(|value| !value.is_empty()),
        content_encoding: "json".into(),
        turn_state_action: "awaiting_response".into(),
        ..NetworkLogDetails::default()
    }
}

pub fn safe_content_encoding(raw: Option<&str>) -> String {
    let value = raw.unwrap_or("none").trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "identity" => "none".into(),
        "gzip" | "br" | "deflate" | "zstd" => value,
        _ => "other".into(),
    }
}

pub fn request_error_kind(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connect"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else {
        "upstream"
    }
    .into()
}

pub fn push(logs: &mut VecDeque<LogEntry>, entry: LogEntry) {
    if entry.flow == "token_fetch" {
        let token_count = logs
            .iter()
            .filter(|existing| existing.flow == "token_fetch")
            .count();
        if token_count >= MAX_TOKEN_FETCH_LOGS {
            if let Some(index) = logs
                .iter()
                .position(|existing| existing.flow == "token_fetch")
            {
                logs.remove(index);
            }
        } else if logs.len() >= MAX_LOGS {
            logs.pop_front();
        }
    } else if logs.len() >= MAX_LOGS {
        logs.pop_front();
    }
    logs.push_back(entry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_origin_keeps_route_and_drops_credentials() {
        assert_eq!(
            endpoint_origin("socks5h://user:secret@127.0.0.1:1080/path?token=hidden"),
            "socks5h://127.0.0.1:1080"
        );
        assert_eq!(
            endpoint_origin("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com:443"
        );
    }

    #[test]
    fn network_details_distinguishes_explicit_and_default_routes() {
        let direct = network_details("https://chatgpt.com/backend-api/codex", "");
        assert_eq!(direct.route_kind, ROUTE_DEFAULT_SYSTEM);
        assert!(direct.proxy_endpoint.is_none());

        let proxied = network_details(
            "https://chatgpt.com/backend-api/codex",
            "http://user:secret@localhost:7897",
        );
        assert_eq!(proxied.route_kind, ROUTE_EXPLICIT_PROXY);
        assert_eq!(
            proxied.proxy_endpoint.as_deref(),
            Some("http://localhost:7897")
        );
    }

    #[test]
    fn token_details_identify_warp_without_exposing_credentials() {
        let details = token_network_details(
            "https://chatgpt.com/backend-api/codex",
            "socks5h://statekit:private@127.0.0.1:1080",
            true,
            "gpt-6-astra",
        );
        assert_eq!(details.flow, "token_fetch");
        assert_eq!(details.route_kind, ROUTE_EMBEDDED_WARP);
        assert_eq!(
            details.proxy_endpoint.as_deref(),
            Some("socks5h://127.0.0.1:1080")
        );
        assert_eq!(details.model.as_deref(), Some("gpt-6-astra"));
    }

    #[test]
    fn token_details_identify_manual_proxy() {
        let details = token_network_details(
            "https://chatgpt.com/backend-api/codex",
            "socks5h://proxy.example.test:44445",
            false,
            "gpt-6-astra",
        );
        assert_eq!(details.route_kind, ROUTE_MANUAL_PROXY);
        assert_eq!(
            details.proxy_endpoint.as_deref(),
            Some("socks5h://proxy.example.test:44445")
        );
    }

    #[test]
    fn safe_text_removes_log_controls_and_bounds_input() {
        assert_eq!(safe_text("  gpt-6\nastra\t-extra", 12), "gpt-6astra-e");
    }

    #[test]
    fn token_fetch_bursts_do_not_evict_all_business_logs() {
        let mut entries = VecDeque::new();
        for _ in 0..60 {
            push(
                &mut entries,
                LogEntry::new(
                    "POST",
                    "/responses",
                    200,
                    Instant::now(),
                    network_details("https://chatgpt.com", ""),
                ),
            );
        }
        for _ in 0..100 {
            push(
                &mut entries,
                LogEntry::new(
                    "POST",
                    "/responses",
                    502,
                    Instant::now(),
                    token_network_details(
                        "https://chatgpt.com",
                        "socks5h://127.0.0.1:1080",
                        true,
                        "gpt-6-astra",
                    ),
                ),
            );
        }

        assert_eq!(entries.len(), MAX_LOGS);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.flow == "token_fetch")
                .count(),
            MAX_TOKEN_FETCH_LOGS
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.flow == "business")
                .count(),
            MAX_LOGS - MAX_TOKEN_FETCH_LOGS
        );
    }
}
