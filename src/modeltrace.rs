//! ModelTrace probe request and Responses output extraction.
//!
//! The scoring implementation and fingerprint bank live in the renderer. This
//! module only performs one probe request through the configured local proxy;
//! it never writes a billing row or stores the model output.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTraceChallenge {
    pub id: String,
    pub expected_count: usize,
    pub prompt: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTraceAttempt {
    pub challenge_id: String,
    pub expected_count: usize,
    pub status: String,
    pub text: Option<String>,
    pub http_status: Option<u16>,
    pub error: Option<String>,
    pub sent_model: String,
}

pub fn extract_output_text(body: &[u8]) -> Result<String> {
    let value: Value = serde_json::from_slice(body).context("解析 Responses 响应")?;
    if let Some(text) = value.get("output_text").and_then(Value::as_str) {
        return Ok(text.to_owned());
    }
    if let Some(text) = value.as_str() {
        return Ok(text.to_owned());
    }

    let mut parts = Vec::new();
    if let Some(output) = value.get("output").and_then(Value::as_array) {
        for item in output {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text.to_owned());
            }
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        parts.push(text.to_owned());
                    } else if let Some(text) = part
                        .get("text")
                        .and_then(|value| value.get("value"))
                        .and_then(Value::as_str)
                    {
                        parts.push(text.to_owned());
                    }
                }
            }
        }
    }
    if let Some(output) = value.get("choices").and_then(Value::as_array) {
        for choice in output {
            if let Some(text) = choice.get("text").and_then(Value::as_str) {
                parts.push(text.to_owned());
            }
            if let Some(text) = choice.pointer("/message/content").and_then(Value::as_str) {
                parts.push(text.to_owned());
            }
            if let Some(content) = choice.pointer("/message/content").and_then(Value::as_array) {
                for part in content {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        parts.push(text.to_owned());
                    } else if let Some(text) = part
                        .get("text")
                        .and_then(|value| value.get("value"))
                        .and_then(Value::as_str)
                    {
                        parts.push(text.to_owned());
                    }
                }
            }
        }
    }
    if parts.is_empty() {
        anyhow::bail!("Responses 响应中没有可归因的文本输出")
    }
    Ok(parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::extract_output_text;

    #[test]
    fn extracts_responses_output_text() {
        let body = br#"{"output":[{"content":[{"type":"output_text","text":"1 2 3"}]}]}"#;
        assert_eq!(extract_output_text(body).unwrap(), "1 2 3");
    }

    #[test]
    fn extracts_chat_completion_content_parts() {
        let body = br#"{"choices":[{"message":{"content":[{"type":"text","text":"1 2"},{"type":"text","text":"3"}]}}]}"#;
        assert_eq!(extract_output_text(body).unwrap(), "1 2\n3");
    }

    #[test]
    fn rejects_responses_without_text() {
        let error = extract_output_text(br#"{"output":[]}"#).unwrap_err();
        assert!(error.to_string().contains("没有可归因"));
    }
}
