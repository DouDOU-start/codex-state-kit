//! Detecting silently downgraded Codex responses.
//!
//! Signals and their meaning follow the official Codex client (`codex-rs`):
//!
//! - `openai-model` / `x-openai-model`: the model that actually served the
//!   turn, as a response header, WebSocket handshake header, or inside
//!   `response.headers` / event `headers`. When it differs from the requested
//!   model the official client emits `ModelReroute` ("this request was routed
//!   to … as a fallback", `Session::maybe_warn_on_server_model_mismatch`),
//!   comparing case-insensitively. This is the only confirmed downgrade.
//! - Safety buffering: `x-codex-safety-buffering-enabled` /
//!   `x-codex-safety-buffering-faster-model` headers, a `safety_buffering`
//!   object on any stream event, or a `response.metadata` event whose metadata
//!   `type` is `safety_buffering` (`reasons`, `use_cases`, `retry_model`).
//!   Upstream holds the turn for extra safety review and the official client
//!   offers "Retry with a faster model"; the turn itself is not rerouted, so it
//!   is only suspected.
//! - `openai_verification_recommendation` in `response.metadata`
//!   (`trusted_access_for_cyber`): the account is flagged and asked to verify,
//!   which is what rerouted accounts see. Suspected.
//! - The response body naming a different model: suspected.
//!
//! The `x-codex-turn-state` length, the `x-codex-primary-used-percent`
//! rate-limit usage and the smallest reasoning `encrypted_content` block are
//! recorded as context only: they do not decide the verdict.

use serde::{Deserialize, Serialize};
use serde_json::Value;

const SAFETY_ENABLED: &str = "x-codex-safety-buffering-enabled";
const SAFETY_FASTER_MODEL: &str = "x-codex-safety-buffering-faster-model";
const TURN_STATE: &str = "x-codex-turn-state";
const PRIMARY_USED: &str = "x-codex-primary-used-percent";
const SECONDARY_USED: &str = "x-codex-secondary-used-percent";
/// Upstream response headers relevant to detection; the HTTP→WebSocket
/// bridge copies these from the handshake onto its synthetic response.
pub const WATCHED_HEADERS: [&str; 7] = [
    SAFETY_ENABLED,
    SAFETY_FASTER_MODEL,
    "openai-model",
    "x-openai-model",
    TURN_STATE,
    PRIMARY_USED,
    SECONDARY_USED,
];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventBuffering {
    reasons: Vec<String>,
    use_cases: Vec<String>,
    /// `Some(None)` when the event explicitly sent `retry_model: null`.
    retry_model: Option<Option<String>>,
}

/// Everything observed for one response.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DowngradeSignals {
    safety_header: bool,
    safety_enabled: Option<bool>,
    faster_model: Option<String>,
    buffering: Option<EventBuffering>,
    served_model: Option<String>,
    #[serde(default)]
    verifications: Vec<String>,
    turn_state_len: Option<usize>,
    primary_used_percent: Option<f64>,
    secondary_used_percent: Option<f64>,
    encrypted_min: Option<usize>,
    encrypted_max: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Upstream reported a different serving model (`openai-model`).
    Confirmed,
    /// Safety buffering, a verification recommendation, or only the response
    /// body names a different model.
    Suspected,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DowngradeReport {
    pub verdict: Verdict,
    pub requested_model: Option<String>,
    /// The model that (likely) served the turn instead.
    pub effective_model: Option<String>,
    pub safety_buffering: bool,
    pub reasons: Vec<String>,
    pub use_cases: Vec<String>,
    /// Faster model upstream offered for a retry while buffering.
    #[serde(default)]
    pub faster_model: Option<String>,
    #[serde(default)]
    pub verifications: Vec<String>,
    pub turn_state_len: Option<usize>,
    pub primary_used_percent: Option<f64>,
    pub encrypted_min: Option<usize>,
    /// Human-readable evidence, strongest first.
    pub signals: Vec<String>,
}

fn json_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        Value::Array(items) => items.iter().find_map(json_string),
        _ => None,
    }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(json_string).collect())
        .unwrap_or_default()
}

/// The official comparison for `openai-model`: case-insensitive equality.
fn same_served_model(requested: &str, served: &str) -> bool {
    requested.trim().eq_ignore_ascii_case(served.trim())
}

/// Same model, or a dated snapshot of it (`gpt-6-astra-2026-05-01`).
pub fn same_model(requested: &str, actual: &str) -> bool {
    let requested = requested.trim().to_ascii_lowercase();
    let actual = actual.trim().to_ascii_lowercase();
    requested == actual || actual.starts_with(&format!("{requested}-"))
}

impl DowngradeSignals {
    fn observe_header(&mut self, name: &str, value: &str) {
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            SAFETY_ENABLED => {
                self.safety_header = true;
                self.safety_enabled = Some(value.eq_ignore_ascii_case("true"));
            }
            SAFETY_FASTER_MODEL => {
                self.safety_header = true;
                if !value.is_empty() {
                    self.faster_model = Some(value.to_string());
                }
            }
            "openai-model" | "x-openai-model" if !value.is_empty() => {
                self.served_model = Some(value.to_string());
            }
            TURN_STATE if !value.is_empty() => self.turn_state_len = Some(value.len()),
            PRIMARY_USED => {
                self.primary_used_percent = value.parse().ok().or(self.primary_used_percent)
            }
            SECONDARY_USED => {
                self.secondary_used_percent = value.parse().ok().or(self.secondary_used_percent)
            }
            _ => {}
        }
    }

    /// Upstream HTTP response (or WebSocket handshake) headers.
    pub fn observe_headers(&mut self, headers: &http::HeaderMap) {
        for (name, value) in headers {
            if let Ok(value) = value.to_str() {
                self.observe_header(name.as_str(), value);
            }
        }
    }

    /// A JSON `headers` object carried inside stream events.
    fn observe_header_json(&mut self, headers: &Value) {
        if let Some(headers) = headers.as_object() {
            for (name, value) in headers {
                if let Some(value) = json_string(value) {
                    self.observe_header(name, &value);
                }
            }
        }
    }

    fn observe_buffering(&mut self, value: &Value) {
        let Some(object) = value.as_object() else {
            return;
        };
        let retry_model = object
            .contains_key("retry_model")
            .then(|| object.get("retry_model").and_then(json_string));
        let next = EventBuffering {
            reasons: strings(object.get("reasons")),
            use_cases: strings(object.get("use_cases")),
            retry_model,
        };
        let merged = match self.buffering.take() {
            Some(mut previous) => {
                for reason in next.reasons {
                    if !previous.reasons.contains(&reason) {
                        previous.reasons.push(reason);
                    }
                }
                for use_case in next.use_cases {
                    if !previous.use_cases.contains(&use_case) {
                        previous.use_cases.push(use_case);
                    }
                }
                if next.retry_model.is_some() {
                    previous.retry_model = next.retry_model;
                }
                previous
            }
            None => next,
        };
        self.buffering = Some(merged);
    }

    fn observe_encrypted(&mut self, item: &Value) {
        if let Some(size) = item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .map(str::len)
        {
            if size > 0 {
                self.encrypted_min = Some(self.encrypted_min.map_or(size, |min| min.min(size)));
                self.encrypted_max = Some(self.encrypted_max.map_or(size, |max| max.max(size)));
            }
        }
    }

    /// One parsed stream event (SSE `data:` payload or WebSocket frame).
    pub fn observe_event(&mut self, event: &Value) {
        if let Some(buffering) = event.get("safety_buffering") {
            self.observe_buffering(buffering);
        }
        if event.get("type").and_then(Value::as_str) == Some("response.metadata") {
            if let Some(metadata) = event.get("metadata") {
                if metadata.get("type").and_then(Value::as_str) == Some("safety_buffering") {
                    self.observe_buffering(metadata);
                }
                for verification in strings(metadata.get("openai_verification_recommendation")) {
                    if !self.verifications.contains(&verification) {
                        self.verifications.push(verification);
                    }
                }
            }
        }
        for headers in [event.get("headers"), event.pointer("/response/headers")]
            .into_iter()
            .flatten()
        {
            self.observe_header_json(headers);
        }
        if let Some(item) = event.get("item") {
            self.observe_encrypted(item);
        }
        if let Some(output) = event.pointer("/response/output").and_then(Value::as_array) {
            for item in output {
                self.observe_encrypted(item);
            }
        }
    }

    /// Judges the response. `sent_model` is what Kit sent upstream,
    /// `response_model` the model named in the response body.
    pub fn report(
        &self,
        sent_model: Option<&str>,
        response_model: Option<&str>,
    ) -> Option<DowngradeReport> {
        let sent = sent_model.map(str::trim).filter(|model| !model.is_empty());
        let mut signals = Vec::new();

        let served_mismatch = match (sent, self.served_model.as_deref()) {
            (Some(sent), Some(served)) => !same_served_model(sent, served),
            _ => false,
        };
        if served_mismatch {
            signals.push(format!(
                "上游响应头 openai-model 为 {}，与请求的 {} 不一致（官方客户端据此提示请求被改路由到备用模型）",
                self.served_model.as_deref().unwrap_or_default(),
                sent.unwrap_or_default()
            ));
        }

        // `enabled: false` alone only advertises the treatment.
        let buffered = self.buffering.is_some() || self.safety_enabled == Some(true);
        let faster_model = match self.buffering.as_ref().and_then(|b| b.retry_model.clone()) {
            // An explicit `retry_model` wins, even null (official precedence).
            Some(explicit) => explicit,
            None => self.faster_model.clone(),
        };
        if buffered {
            let mut text =
                "上游对本次请求启用了安全缓冲（safety buffering），响应被额外审查".to_string();
            if let Some(buffering) = &self.buffering {
                let mut parts = Vec::new();
                if !buffering.use_cases.is_empty() {
                    parts.push(format!("场景 {}", buffering.use_cases.join("/")));
                }
                if !buffering.reasons.is_empty() {
                    parts.push(format!("原因 {}", buffering.reasons.join("/")));
                }
                if !parts.is_empty() {
                    text.push_str(&format!("（{}）", parts.join("，")));
                }
            }
            if let Some(model) = &faster_model {
                text.push_str(&format!("，官方客户端会提示改用更快的 {model} 重试"));
            }
            signals.push(text);
        }

        if !self.verifications.is_empty() {
            signals.push(format!(
                "上游建议账号完成验证：{}（被改路由的账号会收到该提示）",
                self.verifications.join("/")
            ));
        }

        let body_mismatch = match (sent, response_model) {
            (Some(sent), Some(body)) => !same_model(sent, body),
            _ => false,
        };
        if body_mismatch {
            signals.push(format!(
                "响应体返回的模型为 {}，与请求的 {} 不一致",
                response_model.unwrap_or_default(),
                sent.unwrap_or_default()
            ));
        }

        if !(served_mismatch || buffered || body_mismatch || !self.verifications.is_empty()) {
            return None;
        }
        if let Some(len) = self.turn_state_len {
            signals.push(format!("x-codex-turn-state 长度 {len}（仅供参考）"));
        }
        if let Some(percent) = self.primary_used_percent {
            signals.push(format!("主额度已用 {percent}%（仅供参考）"));
        }
        if let Some(size) = self.encrypted_min {
            signals.push(format!("encrypted_content 最小块 {size} 字节（仅供参考）"));
        }
        let effective_model = self
            .served_model
            .clone()
            .filter(|_| served_mismatch)
            .or_else(|| response_model.map(str::to_string).filter(|_| body_mismatch));
        Some(DowngradeReport {
            verdict: if served_mismatch {
                Verdict::Confirmed
            } else {
                Verdict::Suspected
            },
            requested_model: sent.map(str::to_string),
            effective_model,
            safety_buffering: buffered,
            reasons: self
                .buffering
                .as_ref()
                .map(|b| b.reasons.clone())
                .unwrap_or_default(),
            use_cases: self
                .buffering
                .as_ref()
                .map(|b| b.use_cases.clone())
                .unwrap_or_default(),
            faster_model: faster_model.filter(|_| buffered),
            verifications: self.verifications.clone(),
            turn_state_len: self.turn_state_len,
            primary_used_percent: self.primary_used_percent,
            encrypted_min: self.encrypted_min,
            signals,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{HeaderMap, HeaderValue};
    use serde_json::json;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    #[test]
    fn a_clean_astra_turn_is_not_flagged() {
        let mut signals = DowngradeSignals::default();
        signals.observe_headers(&headers(&[
            ("openai-model", "gpt-6-astra"),
            ("x-codex-turn-state", &"a".repeat(292)),
            ("x-codex-primary-used-percent", "12"),
        ]));
        signals.observe_event(&json!({"type": "response.output_item.done", "item": {"type": "reasoning", "encrypted_content": "e".repeat(3529)}}));
        assert_eq!(
            signals.report(Some("gpt-6-astra"), Some("gpt-6-astra")),
            None
        );
    }

    #[test]
    fn safety_buffering_headers_are_only_suspected() {
        // The degraded column of the comparison: buffering on, Luna offered as
        // the faster retry model, 60% primary usage, a 780-char turn state.
        let mut signals = DowngradeSignals::default();
        signals.observe_headers(&headers(&[
            ("x-codex-safety-buffering-enabled", "true"),
            ("x-codex-safety-buffering-faster-model", "gpt-5.6-luna"),
            ("x-codex-primary-used-percent", "60"),
            ("x-codex-turn-state", &"b".repeat(780)),
        ]));
        signals.observe_event(&json!({"type": "response.output_item.done", "item": {"encrypted_content": "e".repeat(1292)}}));
        let report = signals
            .report(Some("gpt-6-astra"), Some("gpt-6-astra"))
            .unwrap();
        assert_eq!(report.verdict, Verdict::Suspected);
        assert!(report.safety_buffering);
        assert_eq!(report.effective_model, None);
        assert_eq!(report.faster_model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(report.turn_state_len, Some(780));
        assert_eq!(report.primary_used_percent, Some(60.0));
        assert_eq!(report.encrypted_min, Some(1292));
        assert!(report.signals[0].contains("gpt-5.6-luna"));
    }

    #[test]
    fn a_rerouted_serving_model_confirms_a_downgrade() {
        let mut signals = DowngradeSignals::default();
        signals.observe_headers(&headers(&[
            ("openai-model", "gpt-5.6-luna"),
            ("x-codex-safety-buffering-enabled", "true"),
            ("x-codex-safety-buffering-faster-model", "gpt-5.6-luna"),
        ]));
        let report = signals
            .report(Some("gpt-6-astra"), Some("gpt-6-astra"))
            .unwrap();
        assert_eq!(report.verdict, Verdict::Confirmed);
        assert_eq!(report.effective_model.as_deref(), Some("gpt-5.6-luna"));
        assert!(report.signals[0].contains("openai-model 为 gpt-5.6-luna"));
    }

    #[test]
    fn an_advertised_but_disabled_treatment_is_not_flagged() {
        let mut signals = DowngradeSignals::default();
        signals.observe_headers(&headers(&[
            ("openai-model", "GPT-6-Astra"),
            ("x-codex-safety-buffering-enabled", "false"),
            ("x-codex-safety-buffering-faster-model", "gpt-5.6-luna"),
        ]));
        assert_eq!(signals.report(Some("gpt-6-astra"), None), None);
    }

    #[test]
    fn a_verification_recommendation_is_suspected() {
        let mut signals = DowngradeSignals::default();
        signals.observe_event(&json!({
            "type": "response.metadata",
            "metadata": {"openai_verification_recommendation": ["trusted_access_for_cyber"]}
        }));
        let report = signals.report(Some("gpt-6-astra"), None).unwrap();
        assert_eq!(report.verdict, Verdict::Suspected);
        assert_eq!(report.verifications, ["trusted_access_for_cyber"]);
    }

    #[test]
    fn stream_events_follow_the_official_retry_model_precedence() {
        let mut signals = DowngradeSignals::default();
        signals.observe_headers(&headers(&[(
            "x-codex-safety-buffering-faster-model",
            "gpt-fast-header",
        )]));
        signals.observe_event(&json!({"type": "response.created", "safety_buffering": false}));
        signals.observe_event(&json!({
            "type": "response.output_text.delta",
            "safety_buffering": {"use_cases": ["cyber"], "reasons": ["user_risk"], "retry_model": "gpt-fast-wire"}
        }));
        let report = signals.report(Some("gpt-6-astra"), None).unwrap();
        assert_eq!(report.faster_model.as_deref(), Some("gpt-fast-wire"));
        assert_eq!(report.use_cases, ["cyber"]);
        assert_eq!(report.reasons, ["user_risk"]);

        // An explicit null retry_model means no fallback model.
        let mut explicit_null = DowngradeSignals::default();
        explicit_null.observe_headers(&headers(&[(
            "x-codex-safety-buffering-faster-model",
            "gpt-fast-header",
        )]));
        explicit_null.observe_event(&json!({"type": "x", "safety_buffering": {"reasons": [], "use_cases": [], "retry_model": null}}));
        assert_eq!(
            explicit_null
                .report(Some("gpt-6-astra"), None)
                .unwrap()
                .faster_model,
            None
        );
    }

    #[test]
    fn websocket_metadata_events_carry_headers_and_buffering() {
        let mut signals = DowngradeSignals::default();
        signals.observe_event(&json!({
            "type": "response.metadata",
            "headers": {"openai-model": "gpt-5.6-luna", "x-codex-turn-state": "c".repeat(292)},
            "metadata": {"type": "safety_buffering", "use_cases": ["bio"], "reasons": ["policy"]}
        }));
        let report = signals.report(Some("gpt-6-astra"), None).unwrap();
        assert_eq!(report.verdict, Verdict::Confirmed);
        assert_eq!(report.use_cases, ["bio"]);
        assert!(report
            .signals
            .iter()
            .any(|s| s.contains("openai-model 为 gpt-5.6-luna")));
    }

    #[test]
    fn turn_state_length_alone_does_not_flag_a_turn() {
        let mut signals = DowngradeSignals::default();
        signals.observe_headers(&headers(&[("x-codex-turn-state", &"d".repeat(780))]));
        assert_eq!(
            signals.report(Some("gpt-6-astra"), Some("gpt-6-astra")),
            None
        );
    }

    #[test]
    fn a_different_body_model_alone_is_only_suspected() {
        let body_only = DowngradeSignals::default();
        let report = body_only
            .report(Some("gpt-6-astra"), Some("gpt-5.6-luna"))
            .unwrap();
        assert_eq!(report.verdict, Verdict::Suspected);
        assert_eq!(report.effective_model.as_deref(), Some("gpt-5.6-luna"));
        assert_eq!(
            body_only.report(Some("gpt-6-astra"), Some("gpt-6-astra-2026-05-01")),
            None
        );
    }
}
