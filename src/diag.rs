use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(not(test))]
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Mutex, OnceLock};

const MAX_BYTES: u64 = 8 * 1024 * 1024;
static WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static REQ_SEQ: AtomicU64 = AtomicU64::new(1);
#[cfg(not(test))]
static WRITE_QUEUE: OnceLock<SyncSender<String>> = OnceLock::new();

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

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub id: u64,
    pub flow: String,
    pub model: Option<String>,
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
    // Diagnostics must never hold up a forwarding task on filesystem I/O.
    // Tests keep the synchronous path so assertions can inspect the file
    // immediately; production uses one bounded writer thread.
    #[cfg(not(test))]
    {
        let sender = WRITE_QUEUE.get_or_init(|| {
            let (sender, receiver) = sync_channel::<String>(2048);
            std::thread::Builder::new()
                .name("codex-state-kit-diag".into())
                .spawn(move || {
                    while let Ok(line) = receiver.recv() {
                        write_line_sync(&line);
                    }
                })
                .expect("spawn diagnostics writer");
            sender
        });
        let _ = sender.try_send(line.to_owned());
        return;
    }
    #[cfg(test)]
    write_line_sync(line);
}

fn write_line_sync(line: &str) {
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
