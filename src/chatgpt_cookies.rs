use http::{header, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

const MAX_COOKIES: usize = 16;
const MAX_NAME_LEN: usize = 64;
const MAX_VALUE_LEN: usize = 4096;
const MAX_HEADER_LEN: usize = 4096;

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

pub const FACTORY_COOKIE_NAME: &str = "oai-chat-psp";
pub const FACTORY_COOKIE_VALUE: &str = "true";

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutingCookie {
    pub name: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<i64>,
}

impl fmt::Debug for RoutingCookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoutingCookie")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("expires_unix", &self.expires_unix)
            .finish()
    }
}

pub fn is_allowed_name(name: &str) -> bool {
    ALLOWED_NAMES.contains(&name) || name.starts_with("cf_chl_")
}

pub fn is_chatgpt_https_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw.trim()) else {
        return false;
    };
    url.scheme() == "https" && url.host_str().is_some_and(is_chatgpt_host)
}

fn is_chatgpt_host(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    matches_host(&host, "chatgpt.com")
        || matches_host(&host, "chat.openai.com")
        || matches_host(&host, "chatgpt-staging.com")
}

fn matches_host(host: &str, apex: &str) -> bool {
    host == apex || host.ends_with(&format!(".{apex}"))
}

pub fn live_cookies(cookies: &[RoutingCookie], now: i64) -> Vec<RoutingCookie> {
    cookies
        .iter()
        .filter(|cookie| cookie.expires_unix.is_none_or(|exp| exp > now))
        .cloned()
        .collect()
}

pub fn cookie_names(cookies: &[RoutingCookie]) -> Vec<String> {
    live_cookies(cookies, chrono::Utc::now().timestamp())
        .into_iter()
        .map(|cookie| cookie.name)
        .collect()
}

#[derive(Clone, Debug, Default)]
pub struct CookieJar {
    cookies: BTreeMap<String, RoutingCookie>,
}

impl CookieJar {
    pub fn from_stored(cookies: &[RoutingCookie]) -> Self {
        let mut jar = Self::default();
        jar.merge_stored(cookies);
        jar
    }

    pub fn merge_stored(&mut self, cookies: &[RoutingCookie]) {
        for cookie in sanitize_list(cookies) {
            self.cookies.insert(cookie.name.clone(), cookie);
        }
        self.trim();
    }

    pub fn ingest_response_headers(&mut self, headers: &HeaderMap, now: i64) {
        for value in headers.get_all(header::SET_COOKIE) {
            let Ok(raw) = value.to_str() else {
                continue;
            };
            match parse_set_cookie(raw, now) {
                SetCookieAction::Store(cookie) => {
                    self.cookies.insert(cookie.name.clone(), cookie);
                }
                SetCookieAction::Delete(name) => {
                    self.cookies.remove(&name);
                }
                SetCookieAction::Ignore => {}
            }
        }
        self.trim();
    }

    pub fn stored(&self) -> Vec<RoutingCookie> {
        self.cookies.values().cloned().collect()
    }

    fn trim(&mut self) {
        while self.cookies.len() > MAX_COOKIES {
            if let Some(name) = self.cookies.keys().next().cloned() {
                self.cookies.remove(&name);
            }
        }
    }
}

enum SetCookieAction {
    Store(RoutingCookie),
    Delete(String),
    Ignore,
}

fn parse_set_cookie(raw: &str, now: i64) -> SetCookieAction {
    let raw = raw.trim();
    if raw.is_empty() {
        return SetCookieAction::Ignore;
    }
    let mut parts = raw.split(';');
    let Some(pair) = parts.next() else {
        return SetCookieAction::Ignore;
    };
    let Some((name, value)) = pair.split_once('=') else {
        return SetCookieAction::Ignore;
    };
    let name = name.trim();
    if !is_allowed_name(name) || !valid_name(name) {
        return SetCookieAction::Ignore;
    }
    let value = value.trim();
    if !valid_value(value) {
        return SetCookieAction::Ignore;
    }
    let mut max_age = None;
    for attr in parts {
        let attr = attr.trim();
        let Some((key, val)) = attr.split_once('=') else {
            continue;
        };
        if key.eq_ignore_ascii_case("max-age") {
            if let Ok(secs) = val.trim().parse::<i64>() {
                max_age = Some(secs);
            }
        }
    }
    if value.is_empty() || max_age.is_some_and(|secs| secs <= 0) {
        return SetCookieAction::Delete(name.to_string());
    }
    SetCookieAction::Store(RoutingCookie {
        name: name.to_string(),
        value: value.to_string(),
        expires_unix: max_age.map(|secs| now.saturating_add(secs)),
    })
}

pub fn sanitize_list(cookies: &[RoutingCookie]) -> Vec<RoutingCookie> {
    let mut out = BTreeMap::new();
    for cookie in cookies {
        if !is_allowed_name(&cookie.name)
            || !valid_name(&cookie.name)
            || !valid_value(&cookie.value)
            || cookie.value.is_empty()
        {
            continue;
        }
        out.insert(cookie.name.clone(), cookie.clone());
        if out.len() == MAX_COOKIES {
            break;
        }
    }
    out.into_values().collect()
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

pub fn request_header(cookies: &[RoutingCookie], include_factory: bool) -> Option<String> {
    cookie_header(cookies, include_factory, chrono::Utc::now().timestamp())
}

fn cookie_header(cookies: &[RoutingCookie], include_factory: bool, now: i64) -> Option<String> {
    let mut pairs = BTreeMap::new();
    for cookie in live_cookies(cookies, now) {
        pairs.insert(cookie.name, cookie.value);
    }
    if include_factory {
        pairs
            .entry(FACTORY_COOKIE_NAME.to_string())
            .or_insert_with(|| FACTORY_COOKIE_VALUE.to_string());
    }
    if pairs.is_empty() {
        return None;
    }
    let header = pairs
        .into_iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    (header.len() <= MAX_HEADER_LEN).then_some(header)
}

pub fn retain_allowed_request_cookies(headers: &mut HeaderMap) {
    let Some(raw) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    else {
        return;
    };
    match filter_request_cookie_header(raw).and_then(|filtered| HeaderValue::from_str(&filtered).ok())
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

pub fn apply_to_headers(headers: &mut HeaderMap, cookies: &[RoutingCookie], include_factory: bool) {
    retain_allowed_request_cookies(headers);
    let mut jar = CookieJar::default();
    if let Some(raw) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
    {
        jar.merge_stored(&parse_request_cookies(raw));
    }
    jar.merge_stored(cookies);
    match request_header(&jar.stored(), include_factory)
        .and_then(|header| HeaderValue::from_str(&header).ok())
    {
        Some(value) => {
            headers.insert(header::COOKIE, value);
        }
        None => {
            headers.remove(header::COOKIE);
        }
    }
}

fn parse_request_cookies(raw: &str) -> Vec<RoutingCookie> {
    raw.split(';')
        .filter_map(|part| {
            let (name, value) = part.split_once('=')?;
            let name = name.trim();
            let value = value.trim();
            if !is_allowed_name(name) || !valid_name(name) || !valid_value(value) || value.is_empty()
            {
                return None;
            }
            Some(RoutingCookie {
                name: name.to_string(),
                value: value.to_string(),
                expires_unix: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, value: &str, exp: Option<i64>) -> RoutingCookie {
        RoutingCookie {
            name: name.into(),
            value: value.into(),
            expires_unix: exp,
        }
    }

    #[test]
    fn allowlist_keeps_routing_cookies_and_drops_session() {
        assert!(is_allowed_name("__oailb"));
        assert!(is_allowed_name("__cflb"));
        assert!(is_allowed_name("cf_chl_opt"));
        assert!(!is_allowed_name("__Secure-next-auth.session-token"));
        assert!(!is_allowed_name("chatgpt_session"));
        assert!(!is_allowed_name("oai-did"));

        let mut headers = HeaderMap::new();
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_static("__oailb=route1; Max-Age=3600; Path=/; Secure"),
        );
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_static("__cflb=edge1; Max-Age=240"),
        );
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_static("__Secure-next-auth.session-token=stolen; Max-Age=3600"),
        );
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_static("chatgpt_session=nope; Max-Age=3600"),
        );
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_static("cf_chl_seq=1; Max-Age=60"),
        );

        let mut jar = CookieJar::default();
        jar.ingest_response_headers(&headers, 1_000);
        let stored = jar.stored();
        assert_eq!(
            stored.iter().map(|cookie| cookie.name.as_str()).collect::<Vec<_>>(),
            vec!["__cflb", "__oailb", "cf_chl_seq"]
        );
        assert_eq!(
            stored.iter().find(|cookie| cookie.name == "__oailb").unwrap().expires_unix,
            Some(4_600)
        );
        assert!(format!("{stored:?}").contains("<redacted>"));
        assert!(!format!("{stored:?}").contains("route1"));
    }

    #[test]
    fn expired_and_deleted_cookies_are_dropped() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_static("__cflb=old; Max-Age=0"),
        );
        headers.append(
            header::SET_COOKIE,
            HeaderValue::from_static("__oailb=; Path=/"),
        );
        let mut jar = CookieJar::from_stored(&[
            cookie("__cflb", "keep", Some(500)),
            cookie("__oailb", "keep", None),
            cookie("__cf_bm", "dead", Some(100)),
        ]);
        jar.ingest_response_headers(&headers, 200);
        assert!(jar.stored().iter().all(|cookie| cookie.name != "__cflb"));
        assert!(jar.stored().iter().all(|cookie| cookie.name != "__oailb"));
        assert_eq!(live_cookies(&jar.stored(), 200), Vec::<RoutingCookie>::new());
    }

    #[test]
    fn kit_auth_override_keeps_routing_cookies_only() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("session=old; __oailb=route1; chatgpt_session=nope"),
        );
        retain_allowed_request_cookies(&mut headers);
        assert_eq!(headers[header::COOKIE], "__oailb=route1");

        apply_to_headers(
            &mut headers,
            &[cookie("__cflb", "edge1", None), cookie("__oailb", "probe", None)],
            true,
        );
        let raw = headers[header::COOKIE].to_str().unwrap();
        assert!(raw.contains("__cflb=edge1"));
        assert!(raw.contains("__oailb=probe"));
        assert!(raw.contains("oai-chat-psp=true"));
        assert!(!raw.contains("session="));
    }

    #[test]
    fn factory_cookie_only_for_official_chatgpt_https() {
        assert!(is_chatgpt_https_url("https://chatgpt.com/backend-api/codex/responses"));
        assert!(is_chatgpt_https_url("https://api.chatgpt.com/backend-api/codex"));
        assert!(!is_chatgpt_https_url("http://chatgpt.com/backend-api/codex"));
        assert!(!is_chatgpt_https_url("https://example.com/responses"));
        assert_eq!(
            request_header(&[], true).as_deref(),
            Some("oai-chat-psp=true")
        );
        assert_eq!(request_header(&[], false), None);
    }
}
