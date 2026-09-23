//! HTTP JSON 与上游 WebSocket `response.create` 帧之间的转换。
//!
//! 上游 WebSocket 每帧是一条裸 JSON。Kit 对仍走 HTTP 的客户端把它包成 SSE `data:` 行。

use serde_json::{json, Value};

/// 只对 https 上游尝试 WebSocket。本地测试用的 `http://` 模拟端必须继续走 reqwest，
/// 否则一次 WebSocket 握手会占掉只能 accept 一次的 TCP 桩。
pub fn should_bridge_http(enabled: bool, method: &str, path: &str, target: &str) -> bool {
    enabled
        && method.eq_ignore_ascii_case("POST")
        && path.contains("/responses")
        && target.starts_with("https://")
}

/// 由设置里的 HTTP 上游地址推导 WebSocket 地址。已是 `/responses` 时不重复拼接。
pub fn upstream_to_ws_url(upstream: &str) -> Result<String, String> {
    let mut url = url::Url::parse(upstream.trim()).map_err(|err| err.to_string())?;
    match url.scheme() {
        "https" => url
            .set_scheme("wss")
            .map_err(|_| "无法把 https 换成 wss".to_string())?,
        "http" => url
            .set_scheme("ws")
            .map_err(|_| "无法把 http 换成 ws".to_string())?,
        "wss" | "ws" => {}
        other => return Err(format!("不支持的上游协议: {other}")),
    }
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/responses") {
        let next = if path.is_empty() {
            "/responses".to_string()
        } else {
            format!("{path}/responses")
        };
        url.set_path(&next);
    }
    Ok(url.to_string())
}

/// 解压 HTTP 请求体并补上 WebSocket 必须的 `type`。
pub fn http_body_to_ws_request(bytes: &[u8]) -> Result<Value, String> {
    let plain = decode_body(bytes)?;
    let mut value: Value =
        serde_json::from_slice(&plain).map_err(|_| "请求体不是 JSON".to_string())?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "请求体不是 JSON 对象".to_string())?;
    if !object.contains_key("type") {
        object.insert("type".to_string(), json!("response.create"));
    }
    Ok(value)
}

pub fn rewrite_model_in_ws_frame(frame: &mut Value, model: &str) {
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    if let Some(object) = frame.as_object_mut() {
        object.insert("model".to_string(), json!(model));
    }
}

pub fn ws_event_to_sse_line(text: &str) -> String {
    let compact = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| serde_json::to_string(&value).ok())
        .unwrap_or_else(|| text.trim().to_string());
    format!("data: {compact}\n\n")
}

pub fn is_terminal_event(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    matches!(
        value.get("type").and_then(Value::as_str),
        Some("response.completed" | "response.failed" | "response.incomplete" | "error")
    )
}

pub fn ws_error_code(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("error") {
        return None;
    }
    value
        .pointer("/error/code")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/error/type").and_then(Value::as_str))
        .map(str::to_string)
}

pub fn strip_previous_response_id(frame: &mut Value) {
    if let Some(object) = frame.as_object_mut() {
        object.remove("previous_response_id");
    }
}

fn decode_body(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() >= 4
        && bytes[0] == 0x28
        && bytes[1] == 0xB5
        && bytes[2] == 0x2F
        && bytes[3] == 0xFD
    {
        return zstd::decode_all(std::io::Cursor::new(bytes))
            .map_err(|_| "无法解压 zstd 请求体".to_string());
    }
    if bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut plain = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut plain)
            .map_err(|_| "无法解压 gzip 请求体".to_string())?;
        return Ok(plain);
    }
    if serde_json::from_slice::<Value>(bytes).is_ok() {
        return Ok(bytes.to_vec());
    }
    let mut decoder = flate2::read::DeflateDecoder::new(bytes);
    let mut plain = Vec::new();
    if std::io::Read::read_to_end(&mut decoder, &mut plain).is_ok() && !plain.is_empty() {
        return Ok(plain);
    }
    Err("请求体不是 JSON".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_json_becomes_response_create() {
        let frame = http_body_to_ws_request(br#"{"model":"codex-mini-latest","stream":true}"#)
            .expect("json");
        assert_eq!(frame["type"], json!("response.create"));
        assert_eq!(frame["model"], json!("codex-mini-latest"));
    }

    #[test]
    fn existing_type_is_kept() {
        let frame = http_body_to_ws_request(br#"{"type":"response.create","model":"m"}"#).unwrap();
        assert_eq!(frame["type"], json!("response.create"));
    }

    #[test]
    fn zstd_body_is_decoded() {
        let raw = serde_json::json!({"model":"from-zstd"});
        let bytes = serde_json::to_vec(&raw).unwrap();
        let compressed = zstd::encode_all(bytes.as_slice(), 3).unwrap();
        let frame = http_body_to_ws_request(&compressed).unwrap();
        assert_eq!(frame["model"], json!("from-zstd"));
        assert_eq!(frame["type"], json!("response.create"));
    }

    #[test]
    fn model_rewrite() {
        let mut frame = json!({"model":"client","type":"response.create"});
        rewrite_model_in_ws_frame(&mut frame, "forced");
        assert_eq!(frame["model"], json!("forced"));
    }

    #[test]
    fn sse_line_and_terminal_events() {
        let line = ws_event_to_sse_line(r#"{"type":"response.completed"}"#);
        assert_eq!(line, "data: {\"type\":\"response.completed\"}\n\n");
        assert!(is_terminal_event(r#"{"type":"response.completed"}"#));
        assert!(is_terminal_event(r#"{"type":"response.failed"}"#));
        assert!(is_terminal_event(r#"{"type":"error"}"#));
        assert!(!is_terminal_event(
            r#"{"type":"response.output_text.delta"}"#
        ));
        assert_eq!(
            ws_error_code(
                r#"{"type":"error","error":{"code":"websocket_connection_limit_reached"}}"#
            )
            .as_deref(),
            Some("websocket_connection_limit_reached")
        );
    }

    #[test]
    fn upstream_url_gains_responses_once() {
        assert_eq!(
            upstream_to_ws_url("https://chatgpt.com/backend-api/codex").unwrap(),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            upstream_to_ws_url("https://chatgpt.com/backend-api/codex/responses").unwrap(),
            "wss://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            upstream_to_ws_url("http://127.0.0.1:9/v1").unwrap(),
            "ws://127.0.0.1:9/v1/responses"
        );
    }

    #[test]
    fn http_upstreams_stay_on_reqwest() {
        assert!(!should_bridge_http(
            true,
            "POST",
            "/responses",
            "http://127.0.0.1:9/responses"
        ));
        assert!(should_bridge_http(
            true,
            "POST",
            "/v1/responses",
            "https://chatgpt.com/backend-api/codex/responses"
        ));
        assert!(!should_bridge_http(
            false,
            "POST",
            "/responses",
            "https://chatgpt.com/backend-api/codex/responses"
        ));
        assert!(!should_bridge_http(
            true,
            "GET",
            "/responses",
            "https://chatgpt.com/backend-api/codex/responses"
        ));
    }

    #[test]
    fn strip_previous_response_id_only_removes_that_field() {
        let mut frame = json!({"previous_response_id":"resp_x","model":"m"});
        strip_previous_response_id(&mut frame);
        assert!(frame.get("previous_response_id").is_none());
        assert_eq!(frame["model"], json!("m"));
    }
}
