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
