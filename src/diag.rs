use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static REQ_SEQ: AtomicU64 = AtomicU64::new(1);

pub fn path() -> PathBuf {
    crate::settings::home_dir().join(if crate::settings::is_dev_mode() {
        ".codex-state-kit-dev-diag.jsonl"
    } else {
        ".codex-state-kit-diag.jsonl"
    })
}

pub fn next_id() -> u64 {
    REQ_SEQ.fetch_add(1, Ordering::Relaxed)
}

pub fn token_fp(token: &str) -> String {
    let token = token.trim();
    if token.is_empty() {
        return "none".into();
    }
    let issued = crate::turn_state::issued_unix(token).unwrap_or(0);
    format!(
        "{}:{}:{:016x}",
        token.len(),
        issued,
        fnv1a64(token.as_bytes())
    )
}

pub fn token_age_secs(token: &str) -> Option<i64> {
    crate::turn_state::issued_unix(token.trim())
        .map(|issued| chrono::Utc::now().timestamp().saturating_sub(issued))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub id: u64,
    pub flow: String,
    pub model: Option<String>,
    pub same_turn: bool,
    pub client_had_state: bool,
    pub token_fp: Option<String>,
    pub token_age_secs: Option<i64>,
    pub cookies: Vec<String>,
    pub turn_state_action: String,
    pub route_kind: String,
    pub proxy_session: Option<String>,
}

pub fn emit(stage: &str, req: Option<&Request>, extra: Value) {
    let mut line = json!({
        "ts": chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
        "stage": stage,
    });
    let Some(object) = line.as_object_mut() else {
        return;
    };
    if let Some(req) = req {
        object.insert("id".into(), json!(req.id));
        if !req.flow.is_empty() {
            object.insert("flow".into(), json!(req.flow));
        }
        if let Some(model) = &req.model {
            object.insert("model".into(), json!(model));
        }
        object.insert("sameTurn".into(), json!(req.same_turn));
        object.insert("clientHadState".into(), json!(req.client_had_state));
        if let Some(token_fp) = &req.token_fp {
            object.insert("tokenFp".into(), json!(token_fp));
        }
        if let Some(age) = req.token_age_secs {
            object.insert("tokenAgeSecs".into(), json!(age));
        }
        if !req.cookies.is_empty() {
            object.insert("cookies".into(), json!(req.cookies));
        }
        if !req.turn_state_action.is_empty() {
            object.insert("turnStateAction".into(), json!(req.turn_state_action));
        }
        if !req.route_kind.is_empty() {
            object.insert("routeKind".into(), json!(req.route_kind));
        }
        if let Some(session) = &req.proxy_session {
            object.insert("session".into(), json!(session));
        }
    }
    if let Some(extra) = extra.as_object() {
        for (key, value) in extra {
            object.insert(key.clone(), value.clone());
        }
    }
    write_line(&line.to_string());
}

fn write_line(line: &str) {
    let _guard = WRITE_LOCK.get_or_init(|| Mutex::new(())).lock();
    let path = path();
    if let Ok(meta) = fs::metadata(&path) {
        if meta.len() > MAX_BYTES {
            let rotated = path.with_extension("jsonl.1");
            let _ = fs::rename(&path, rotated);
        }
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE;
    use base64::Engine as _;

    fn token_for(issued: i64) -> String {
        let mut raw = vec![0x80];
        raw.extend_from_slice(&issued.to_be_bytes());
        raw.extend_from_slice(&[0u8; 210]);
        URL_SAFE.encode(raw)
    }

    #[test]
    fn token_fingerprint_is_stable_and_does_not_embed_raw_token() {
        let token = token_for(1_700_000_000);
        let first = token_fp(&token);
        let second = token_fp(&token);
        assert_eq!(first, second);
        assert!(first.starts_with(&format!("{}:1700000000:", token.len())));
        assert!(!first.contains(&token));
        assert_ne!(token_fp(&token_for(1_700_000_001)), first);
    }
}
