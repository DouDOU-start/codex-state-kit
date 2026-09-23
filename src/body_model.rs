//! 从请求体读写 `model`。强制绑定模型时用，和 Turn-State 无关。

use serde_json::Value;

pub fn extract_model_from_body(bytes: &[u8]) -> Option<String> {
    if let Some(model) = extract_model_from_json(bytes) {
        return Some(model);
    }
    if bytes.len() >= 4
        && bytes[0] == 0x28
        && bytes[1] == 0xB5
        && bytes[2] == 0x2F
        && bytes[3] == 0xFD
    {
        if let Ok(decompressed) = zstd::decode_all(std::io::Cursor::new(bytes)) {
            if let Some(model) = extract_model_from_json(&decompressed) {
                return Some(model);
            }
        }
    }
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        if let Some(model) = try_decompress_and_extract(bytes, "gzip") {
            return Some(model);
        }
    }
    if let Some(model) = try_decompress_and_extract(bytes, "deflate") {
        return Some(model);
    }
    try_decompress_and_extract(bytes, "raw_deflate")
}

pub fn rewrite_model_in_body(
    bytes: &[u8],
    encoding: Option<&str>,
    model: &str,
) -> Result<Vec<u8>, String> {
    let model = model.trim();
    if model.is_empty() {
        return Ok(bytes.to_vec());
    }
    if extract_model_from_json(bytes).as_deref() == Some(model) {
        return Ok(bytes.to_vec());
    }
    match detect_body_compression(bytes, encoding) {
        None => rewrite_json_model(bytes, model),
        Some("zstd") => {
            let plain = zstd::decode_all(std::io::Cursor::new(bytes))
                .map_err(|_| "无法解压 zstd 请求体，不能强制绑定模型".to_string())?;
            if extract_model_from_json(&plain).as_deref() == Some(model) {
                return Ok(bytes.to_vec());
            }
            let rewritten = rewrite_json_model(&plain, model)?;
            zstd::encode_all(rewritten.as_slice(), 0)
                .map_err(|_| "无法重新压缩 zstd 请求体".to_string())
        }
        Some("gzip") => {
            let plain = decompress_named(bytes, "gzip")
                .ok_or_else(|| "无法解压 gzip 请求体，不能强制绑定模型".to_string())?;
            if extract_model_from_json(&plain).as_deref() == Some(model) {
                return Ok(bytes.to_vec());
            }
            compress_gzip(&rewrite_json_model(&plain, model)?)
        }
        Some("deflate") => {
            let plain = decompress_named(bytes, "deflate")
                .or_else(|| decompress_named(bytes, "raw_deflate"))
                .ok_or_else(|| "无法解压 deflate 请求体，不能强制绑定模型".to_string())?;
            if extract_model_from_json(&plain).as_deref() == Some(model) {
                return Ok(bytes.to_vec());
            }
            compress_deflate(&rewrite_json_model(&plain, model)?)
        }
        Some(_) => Err("不支持的请求体压缩，不能强制绑定模型".into()),
    }
}

/// 读取请求体顶层的字符串字段（如 `service_tier`），支持 zstd / gzip / deflate。
pub fn extract_str_field(bytes: &[u8], encoding: Option<&str>, key: &str) -> Option<String> {
    let plain = match detect_body_compression(bytes, encoding) {
        None => bytes.to_vec(),
        Some("zstd") => zstd::decode_all(std::io::Cursor::new(bytes)).ok()?,
        Some("gzip") => decompress_named(bytes, "gzip")?,
        Some("deflate") => {
            decompress_named(bytes, "deflate").or_else(|| decompress_named(bytes, "raw_deflate"))?
        }
        Some(_) => return None,
    };
    serde_json::from_slice::<Value>(&plain)
        .ok()?
        .get(key)?
        .as_str()
        .map(str::to_owned)
}

/// Codex HTTP `/responses` 不接受 `previous_response_id`（只有 WebSocket 链式续跑会用），
/// 走 HTTP 转发前去掉它，否则上游返回 `Invalid previous_response_id`。
/// 字段不存在、正文无法解析或无法重新压缩时原样返回。
pub fn strip_previous_response_id(bytes: &[u8], encoding: Option<&str>) -> Vec<u8> {
    let stripped = match detect_body_compression(bytes, encoding) {
        None => strip_previous_response_id_json(bytes),
        Some("zstd") => zstd::decode_all(std::io::Cursor::new(bytes))
            .ok()
            .and_then(|plain| strip_previous_response_id_json(&plain))
            .and_then(|plain| zstd::encode_all(plain.as_slice(), 0).ok()),
        Some("gzip") => decompress_named(bytes, "gzip")
            .and_then(|plain| strip_previous_response_id_json(&plain))
            .and_then(|plain| compress_gzip(&plain).ok()),
        Some("deflate") => decompress_named(bytes, "deflate")
            .or_else(|| decompress_named(bytes, "raw_deflate"))
            .and_then(|plain| strip_previous_response_id_json(&plain))
            .and_then(|plain| compress_deflate(&plain).ok()),
        Some(_) => None,
    };
    stripped.unwrap_or_else(|| bytes.to_vec())
}

/// 字段不存在或正文不是 JSON 对象时返回 `None`，表示无需改写。
fn strip_previous_response_id_json(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(bytes).ok()?;
    value.as_object_mut()?.remove("previous_response_id")?;
    serde_json::to_vec(&value).ok()
}

fn detect_body_compression(bytes: &[u8], encoding: Option<&str>) -> Option<&'static str> {
    let encoding = encoding.unwrap_or("").to_ascii_lowercase();
    if encoding.contains("zstd")
        || (bytes.len() >= 4
            && bytes[0] == 0x28
            && bytes[1] == 0xB5
            && bytes[2] == 0x2F
            && bytes[3] == 0xFD)
    {
        return Some("zstd");
    }
    if encoding.contains("gzip") || (bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B) {
        return Some("gzip");
    }
    if encoding.contains("deflate") {
        return Some("deflate");
    }
    None
}

fn rewrite_json_model(bytes: &[u8], model: &str) -> Result<Vec<u8>, String> {
    let mut value: Value = serde_json::from_slice(bytes)
        .map_err(|_| "请求体不是 JSON，不能强制绑定模型".to_string())?;
    let Some(object) = value.as_object_mut() else {
        return Err("请求体不是 JSON 对象，不能强制绑定模型".into());
    };
    object.insert("model".into(), Value::String(model.to_string()));
    serde_json::to_vec(&value).map_err(|_| "无法序列化改写后的请求体".to_string())
}

fn decompress_named(bytes: &[u8], method: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut decompressed = Vec::new();
    let ok = match method {
        "gzip" => flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut decompressed)
            .is_ok(),
        "deflate" => flate2::read::ZlibDecoder::new(bytes)
            .read_to_end(&mut decompressed)
            .is_ok(),
        "raw_deflate" => flate2::read::DeflateDecoder::new(bytes)
            .read_to_end(&mut decompressed)
            .is_ok(),
        _ => false,
    };
    (ok && !decompressed.is_empty()).then_some(decompressed)
}

fn compress_gzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(bytes)
        .and_then(|_| encoder.finish())
        .map_err(|_| "无法重新压缩 gzip 请求体".to_string())
}

fn compress_deflate(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(bytes)
        .and_then(|_| encoder.finish())
        .map_err(|_| "无法重新压缩 deflate 请求体".to_string())
}

fn extract_model_from_json(bytes: &[u8]) -> Option<String> {
    if let Ok(value) = serde_json::from_slice::<Value>(bytes) {
        if let Some(model) = value.get("model").and_then(|item| item.as_str()) {
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
    }
    let text = std::str::from_utf8(bytes).ok()?;
    let needle = "\"model\"";
    let idx = text.find(needle)?;
    let after = &text[idx + needle.len()..];
    let colon_pos = after.find(':')?;
    let after_colon = after[colon_pos + 1..].trim_start();
    if after_colon.starts_with('"') {
        let end = after_colon[1..].find('"')?;
        let model = &after_colon[1..1 + end];
        if !model.is_empty() {
            return Some(model.to_string());
        }
    }
    None
}

fn try_decompress_and_extract(bytes: &[u8], method: &str) -> Option<String> {
    decompress_named(bytes, method).and_then(|plain| extract_model_from_json(&plain))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_str_field_reads_plain_and_compressed_bodies() {
        let plain = br#"{"model":"gpt-5.1-codex","service_tier":"priority"}"#;
        assert_eq!(
            extract_str_field(plain, None, "service_tier").as_deref(),
            Some("priority")
        );
        let compressed = zstd::encode_all(plain.as_slice(), 3).unwrap();
        assert_eq!(
            extract_str_field(&compressed, Some("zstd"), "service_tier").as_deref(),
            Some("priority")
        );
        assert_eq!(extract_str_field(plain, None, "missing"), None);
    }

    #[test]
    fn strip_previous_response_id_removes_only_that_field() {
        let input = br#"{"model":"gpt-6-astra","previous_response_id":"resp_1","input":[]}"#;
        let value: Value =
            serde_json::from_slice(&strip_previous_response_id(input, None)).unwrap();
        assert!(value.get("previous_response_id").is_none());
        assert_eq!(
            value.get("model").and_then(Value::as_str),
            Some("gpt-6-astra")
        );
        assert!(value.get("input").is_some());
    }

    #[test]
    fn strip_previous_response_id_keeps_bytes_when_absent_or_invalid() {
        let absent = br#"{"model":"gpt-6-astra","input":[]}"#;
        assert_eq!(strip_previous_response_id(absent, None), absent);
        let invalid = b"not-json-at-all";
        assert_eq!(strip_previous_response_id(invalid, None), invalid);
    }

    #[test]
    fn strip_previous_response_id_handles_compressed_bodies() {
        let plain = br#"{"model":"gpt-6-astra","previous_response_id":"resp_z","input":[]}"#;
        let cases = [
            ("zstd", zstd::encode_all(plain.as_slice(), 3).unwrap()),
            ("gzip", compress_gzip(plain).unwrap()),
            ("deflate", compress_deflate(plain).unwrap()),
        ];
        for (encoding, compressed) in cases {
            let result = strip_previous_response_id(&compressed, Some(encoding));
            let decompressed = match encoding {
                "zstd" => zstd::decode_all(std::io::Cursor::new(&result)).unwrap(),
                other => decompress_named(&result, other).unwrap(),
            };
            let value: Value = serde_json::from_slice(&decompressed).unwrap();
            assert!(value.get("previous_response_id").is_none(), "{encoding}");
            assert_eq!(
                value.get("model").and_then(Value::as_str),
                Some("gpt-6-astra"),
                "{encoding}"
            );
        }
    }
}
