use http::{header, HeaderMap, HeaderValue};
use std::collections::BTreeMap;

const MAX_NAME_LEN: usize = 64;
const MAX_VALUE_LEN: usize = 4096;

/// 与官方 Codex `ChatGptCloudflareCookieStore` 对齐：只收线路 / Cloudflare cookie。
const ALLOWED_NAMES: &[&str] = &[
    "__cf_bm",
    "__cflb",
    "__cfruid",
    "__cfseq",
    "__cfwaitingroom",
    "__oailb",
    "_cfuvid",
    "cf_clearance",
    "cf_ob_info",
    "cf_use_ob",
];

const FACTORY_COOKIE_NAME: &str = "oai-chat-psp";

fn is_allowed_name(name: &str) -> bool {
    ALLOWED_NAMES.contains(&name) || name.starts_with("cf_chl_")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn valid_value(value: &str) -> bool {
    value.len() <= MAX_VALUE_LEN
        && value
            .chars()
            .all(|ch| ch.is_ascii() && !ch.is_ascii_control() && ch != ';')
}

/// Keeps only routing / Cloudflare cookies on a request whose auth Kit
/// replaces, so the client's own session cookies never go upstream.
pub fn retain_allowed_request_cookies(headers: &mut HeaderMap) {
    let Some(raw) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    else {
        return;
    };
    match filter_request_cookie_header(raw)
        .and_then(|filtered| HeaderValue::from_str(&filtered).ok())
    {
        Some(value) => {
            headers.insert(header::COOKIE, value);
        }
        None => {
            headers.remove(header::COOKIE);
        }
    }
}

fn filter_request_cookie_header(raw: &str) -> Option<String> {
    let mut pairs = BTreeMap::new();
    for part in raw.split(';') {
        let Some((name, value)) = part.split_once('=') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        let allowed = name == FACTORY_COOKIE_NAME || (is_allowed_name(name) && valid_name(name));
        if !allowed || !valid_value(value) || value.is_empty() {
            continue;
        }
        pairs.insert(name.to_string(), value.to_string());
    }
    if pairs.is_empty() {
        return None;
    }
    Some(
        pairs
            .into_iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kit_auth_override_keeps_routing_cookies_only() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static(
                "session=old; __oailb=route1; chatgpt_session=nope; oai-chat-psp=true",
            ),
        );
        retain_allowed_request_cookies(&mut headers);
        assert_eq!(headers[header::COOKIE], "__oailb=route1; oai-chat-psp=true");

        let mut only_session = HeaderMap::new();
        only_session.insert(header::COOKIE, HeaderValue::from_static("session=old"));
        retain_allowed_request_cookies(&mut only_session);
        assert!(only_session.get(header::COOKIE).is_none());
    }
}
