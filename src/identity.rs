//! Kit 对外呈现的一台装着 Codex CLI 的机器。
//!
//! `installation_id` 写在磁盘上，重启后不变。`session_id`、`window_id` 和 `thread_id`
//! 每次进程启动重新生成。探针、业务 HTTP 和上游 WebSocket 都用这一份，不透传客户端自己的设备头。
//!
//! 系统只能在 Mac / Windows / Linux 三个预设里选。系统版本、架构和终端跟着预设走，
//! 取值与官方 CLI 在对应系统上用 `os_info` 和终端检测得到的一致，避免拼出不存在的组合
//! （例如 Windows 配 macOS 的版本号）。CLI 版本跟随本机安装的 codex，originator 固定。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::settings::{home_dir, is_dev_mode};

const DEFAULT_VERSION: &str = "0.155.0";
const ORIGINATOR: &str = "codex_cli_rs";

/// The operating system the virtual device reports.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DevicePlatform {
    #[default]
    Mac,
    Windows,
    Linux,
}

/// What the official CLI reports on a platform: `(os_type, os_version, arch, terminal)`.
/// Mac: macOS 15.5 on Apple silicon. Windows: Windows 11 24H2 (os_info reports
/// `10.0.<build>`) in Windows Terminal. Linux: Ubuntu 24.04, which os_info
/// reports by distribution as `Ubuntu 24.4.0`.
impl DevicePlatform {
    fn preset(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::Mac => ("Mac OS", "15.5.0", "arm64", "xterm-256color"),
            Self::Windows => ("Windows", "10.0.26100", "x86_64", "WindowsTerminal"),
            Self::Linux => ("Ubuntu", "24.4.0", "x86_64", "xterm-256color"),
        }
    }

    fn of(os_type: &str) -> Self {
        match os_type.trim() {
            "Mac OS" => Self::Mac,
            "Windows" => Self::Windows,
            _ => Self::Linux,
        }
    }
}

const IDENTITY_HEADERS: &[&str] = &[
    "x-codex-installation-id",
    "x-codex-routing-hint",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-codex-parent-thread-id",
    "x-openai-subagent",
    "session_id",
    "originator",
    "version",
    "user-agent",
    "thread-id",
    "x-client-request-id",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmIdentity {
    #[serde(default)]
    pub environment: VirtualEnvironment,
    pub installation_id: String,
    pub cli_version: String,
    pub originator: String,
    pub os_type: String,
    pub os_version: String,
    pub arch: String,
    pub terminal: String,
    #[serde(skip)]
    pub session_id: String,
    #[serde(skip)]
    pub window_id: String,
    #[serde(skip)]
    pub thread_id: String,
}

/// The only user choice for a virtual device; everything else follows it.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmProfile {
    pub platform: DevicePlatform,
    pub environment: Option<VirtualEnvironment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VirtualEnvironment {
    #[serde(default = "automatic_region")]
    pub auto_region: bool,
    #[serde(default)]
    pub timezone: String,
    #[serde(default)]
    pub locale: String,
    #[serde(default)]
    pub region: String,
}

fn automatic_region() -> bool {
    true
}

impl Default for VirtualEnvironment {
    fn default() -> Self {
        Self {
            auto_region: true,
            timezone: String::new(),
            locale: String::new(),
            region: String::new(),
        }
    }
}

impl VirtualEnvironment {
    pub fn validate(&self) -> Result<()> {
        if !self.timezone.is_empty() {
            self.timezone
                .parse::<chrono_tz::Tz>()
                .context("请输入有效的 IANA 时区，例如 Asia/Tokyo")?;
        }
        anyhow::ensure!(
            self.locale.len() <= 35
                && self
                    .locale
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "语言标签只允许字母、数字和连字符"
        );
        anyhow::ensure!(
            self.region.len() <= 2 && self.region.chars().all(|c| c.is_ascii_uppercase()),
            "地区需使用两位大写国家代码"
        );
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VmIdentityView {
    pub environment: VirtualEnvironment,
    pub installation_id: String,
    pub session_id: String,
    pub platform: DevicePlatform,
    pub cli_version: String,
    pub originator: String,
    pub os_type: String,
    pub os_version: String,
    pub arch: String,
    pub terminal: String,
    pub user_agent: String,
}

impl VmIdentity {
    pub fn load_or_create() -> Self {
        let path = identity_path();
        let mut identity = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str::<VmIdentity>(&raw).unwrap_or_else(|_| Self::fresh()),
            Err(_) => Self::fresh(),
        };
        if !valid_uuid(&identity.installation_id) {
            identity.installation_id = Uuid::new_v4().to_string();
        }
        identity.normalize();
        identity.fill_runtime();
        #[cfg(not(test))]
        if let Some(version) = detect_local_cli_version() {
            identity.cli_version = version;
        }
        if identity.save_to(&path).is_err() {
            eprintln!("[identity] 无法写入 {}", path.display());
        }
        identity
    }

    /// 测试和不落盘的临时身份：默认的 Mac 预设。
    pub fn ephemeral() -> Self {
        let mut identity = Self::fresh();
        identity.fill_runtime();
        identity
    }

    pub fn user_agent(&self) -> String {
        format!(
            "{}/{} ({} {}; {}) {}",
            self.originator,
            self.cli_version,
            self.os_type,
            self.os_version,
            self.arch,
            self.terminal
        )
    }

    pub fn routing_hint(&self, model: &str) -> String {
        format!("model={}", model.trim())
    }

    pub fn view(&self) -> VmIdentityView {
        VmIdentityView {
            environment: self.environment.clone(),
            installation_id: self.installation_id.clone(),
            session_id: self.session_id.clone(),
            platform: self.platform(),
            cli_version: self.cli_version.clone(),
            originator: self.originator.clone(),
            os_type: self.os_type.clone(),
            os_version: self.os_version.clone(),
            arch: self.arch.clone(),
            terminal: self.terminal.clone(),
            user_agent: self.user_agent(),
        }
    }

    pub fn platform(&self) -> DevicePlatform {
        DevicePlatform::of(&self.os_type)
    }

    pub fn apply_profile(&mut self, profile: VmProfile) {
        self.set_platform(profile.platform);
        if let Some(environment) = profile.environment {
            self.environment = environment;
        }
    }

    fn set_platform(&mut self, platform: DevicePlatform) {
        let (os_type, os_version, arch, terminal) = platform.preset();
        self.os_type = os_type.into();
        self.os_version = os_version.into();
        self.arch = arch.into();
        self.terminal = terminal.into();
        self.originator = ORIGINATOR.into();
    }

    /// Brings a stored identity (possibly hand-edited by an older version)
    /// back to its platform's preset.
    fn normalize(&mut self) {
        self.set_platform(self.platform());
        if !is_version(self.cli_version.trim()) {
            self.cli_version = DEFAULT_VERSION.into();
        }
    }

    /// The same device profile on a new machine: new installation and
    /// session ids. Used when an account gets its own virtual device.
    pub fn renewed(&self) -> Self {
        let mut next = self.clone();
        next.installation_id = Uuid::new_v4().to_string();
        next.fill_runtime();
        next
    }

    /// A stored identity (runtime ids are not serialized) ready for use.
    pub fn with_runtime_ids(mut self) -> Self {
        if !valid_uuid(&self.installation_id) {
            self.installation_id = Uuid::new_v4().to_string();
        }
        self.normalize();
        self.fill_runtime();
        self
    }

    pub fn regenerate_installation_id(&mut self) {
        self.installation_id = Uuid::new_v4().to_string();
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&identity_path())
    }

    fn fresh() -> Self {
        let mut identity = Self {
            environment: VirtualEnvironment::default(),
            installation_id: Uuid::new_v4().to_string(),
            cli_version: DEFAULT_VERSION.into(),
            originator: String::new(),
            os_type: String::new(),
            os_version: String::new(),
            arch: String::new(),
            terminal: String::new(),
            session_id: String::new(),
            window_id: String::new(),
            thread_id: String::new(),
        };
        identity.set_platform(DevicePlatform::Mac);
        identity
    }

    fn fill_runtime(&mut self) {
        self.session_id = Uuid::new_v4().to_string();
        self.window_id = Uuid::new_v4().to_string();
        self.thread_id = Uuid::new_v4().to_string();
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let raw = serde_json::to_string_pretty(self).context("序列化虚拟设备身份")?;
        std::fs::write(path, raw).with_context(|| format!("写入 {}", path.display()))?;
        Ok(())
    }
}

pub fn identity_path() -> PathBuf {
    let name = if is_dev_mode() {
        ".codex-state-kit-dev-vm.json"
    } else {
        ".codex-state-kit-vm.json"
    };
    home_dir().join(name)
}

pub fn is_vm_identity_header(name: &str) -> bool {
    IDENTITY_HEADERS
        .iter()
        .any(|header| header.eq_ignore_ascii_case(name))
}

/// Only a standalone harness environment message is eligible. Never rewrite
/// instructions, user prose, tool output, paths or the real execution shell.
fn rewrite_environment(body: &mut Value, identity: &VmIdentity) -> bool {
    let environment = &identity.environment;
    let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for message in input.iter_mut().rev() {
        if message.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for item in content {
            if item.get("type").and_then(Value::as_str) != Some("input_text") {
                continue;
            }
            let Some(text) = item.get_mut("text") else {
                continue;
            };
            let Some(original) = text.as_str() else {
                continue;
            };
            let trimmed = original.trim();
            if !trimmed.starts_with("<environment_context>")
                || !trimmed.ends_with("</environment_context>")
                || trimmed.matches("<environment_context>").count() != 1
            {
                continue;
            }
            let mut next = original.to_string();
            set_context_tag(
                &mut next,
                "virtual_device",
                &format!(
                    "{} {}; {}; {}",
                    identity.os_type, identity.os_version, identity.arch, identity.terminal
                ),
            );
            if let Ok(tz) = environment.timezone.parse::<chrono_tz::Tz>() {
                set_context_tag(&mut next, "timezone", &environment.timezone);
                let date = chrono::Utc::now()
                    .with_timezone(&tz)
                    .format("%Y-%m-%d")
                    .to_string();
                set_context_tag(&mut next, "current_date", &date);
            }
            if !environment.locale.is_empty() && environment.validate().is_ok() {
                set_context_tag(&mut next, "locale", &environment.locale);
            }
            if next != original {
                *text = Value::String(next);
                changed = true;
            }
            return changed;
        }
    }
    changed
}

fn set_context_tag(text: &mut String, name: &str, value: &str) {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    if let Some(start) = text.find(&open) {
        let start = start + open.len();
        if let Some(end) = text[start..].find(&close) {
            text.replace_range(start..start + end, value);
        }
    } else if let Some(end) = text.rfind("</environment_context>") {
        text.insert_str(end, &format!("  {open}{value}{close}\n"));
    }
}

pub fn rewrite_client_metadata_value(body: &mut Value, identity: &VmIdentity) -> bool {
    let environment_changed = rewrite_environment(body, identity);
    let Some(metadata) = body
        .get_mut("client_metadata")
        .and_then(Value::as_object_mut)
    else {
        return environment_changed;
    };
    metadata.insert(
        "x-codex-installation-id".into(),
        json!(identity.installation_id),
    );
    metadata.insert("session_id".into(), json!(identity.session_id));
    metadata.insert("x-codex-window-id".into(), json!(identity.window_id));
    for (key, value) in [
        ("installation_id", &identity.installation_id),
        ("window_id", &identity.window_id),
    ] {
        if metadata.contains_key(key) {
            metadata.insert(key.into(), json!(value));
        }
    }
    // Newer core versions carry the authoritative snapshot as a JSON string.
    if let Some(raw) = metadata
        .get("x-codex-turn-metadata")
        .and_then(Value::as_str)
    {
        if let Ok(Value::Object(mut snapshot)) = serde_json::from_str::<Value>(raw) {
            for (key, value) in [
                ("installation_id", &identity.installation_id),
                ("session_id", &identity.session_id),
                ("window_id", &identity.window_id),
            ] {
                if snapshot.contains_key(key) {
                    snapshot.insert(key.into(), json!(value));
                }
            }
            metadata.insert(
                "x-codex-turn-metadata".into(),
                Value::String(Value::Object(snapshot).to_string()),
            );
        }
    }
    true
}

/// 解压 JSON 正文，替换 `client_metadata` 里的设备字段，再按原编码写回。
/// 同时改写最近的独立 environment_context。没有适用字段时保留原正文。
pub fn rewrite_client_metadata_in_body(
    bytes: &[u8],
    encoding: Option<&str>,
    identity: &VmIdentity,
) -> Result<Vec<u8>, String> {
    let (plain, codec) = decode_body(bytes, encoding)?;
    let mut value: Value = match serde_json::from_slice(&plain) {
        Ok(value) => value,
        Err(_) => return Ok(bytes.to_vec()),
    };
    if !rewrite_client_metadata_value(&mut value, identity) {
        return Ok(bytes.to_vec());
    }
    let encoded = serde_json::to_vec(&value).map_err(|_| "无法序列化改写后的请求体".to_string())?;
    match codec {
        None => Ok(encoded),
        Some("zstd") => zstd::encode_all(encoded.as_slice(), 3)
            .map_err(|_| "无法重新压缩 zstd 请求体".to_string()),
        Some("gzip") => compress_gzip(&encoded),
        Some("deflate") => compress_deflate(&encoded),
        Some(_) => Err("不支持的请求体压缩，不能改写设备身份".into()),
    }
}

pub fn detect_local_cli_version() -> Option<String> {
    let mut child = std::process::Command::new("codex")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut stdout = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    use std::io::Read;
                    pipe.read_to_string(&mut stdout).ok()?;
                }
                return parse_cli_version(&stdout);
            }
            Ok(None) if started.elapsed() < Duration::from_secs(2) => {
                std::thread::sleep(Duration::from_millis(40));
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
}

fn parse_cli_version(text: &str) -> Option<String> {
    let mut version = String::new();
    for token in text.split_whitespace() {
        let token = token.trim_matches(|ch: char| !ch.is_ascii_digit() && ch != '.');
        if is_version(token) {
            version = token.to_string();
            break;
        }
    }
    (!version.is_empty()).then_some(version)
}

fn decode_body(
    bytes: &[u8],
    encoding: Option<&str>,
) -> Result<(Vec<u8>, Option<&'static str>), String> {
    let hinted = encoding.unwrap_or("").trim().to_ascii_lowercase();
    if bytes.len() >= 4
        && bytes[0] == 0x28
        && bytes[1] == 0xB5
        && bytes[2] == 0x2F
        && bytes[3] == 0xFD
        || hinted == "zstd"
    {
        let plain = zstd::decode_all(std::io::Cursor::new(bytes))
            .map_err(|_| "无法解压 zstd 请求体".to_string())?;
        return Ok((plain, Some("zstd")));
    }
    if bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B || hinted == "gzip" {
        let mut decoder = flate2::read::GzDecoder::new(bytes);
        let mut plain = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut plain)
            .map_err(|_| "无法解压 gzip 请求体".to_string())?;
        return Ok((plain, Some("gzip")));
    }
    if hinted == "deflate" {
        let mut decoder = flate2::read::DeflateDecoder::new(bytes);
        let mut plain = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut plain)
            .map_err(|_| "无法解压 deflate 请求体".to_string())?;
        return Ok((plain, Some("deflate")));
    }
    Ok((bytes.to_vec(), None))
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
    let mut encoder =
        flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(bytes)
        .and_then(|_| encoder.finish())
        .map_err(|_| "无法重新压缩 deflate 请求体".to_string())
}

fn is_version(value: &str) -> bool {
    let mut parts = value.split('.');
    let ok = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.chars().all(|ch| ch.is_ascii_digit()))
    });
    ok && parts.next().is_none()
}

fn valid_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_rewrites_only_latest_harness_message_without_metadata() {
        let mut identity = VmIdentity::ephemeral();
        identity.environment = VirtualEnvironment {
            auto_region: false,
            timezone: "America/Los_Angeles".into(),
            locale: "en-US".into(),
            region: "US".into(),
        };
        let context = "<environment_context>\n<cwd>E:\\code</cwd><shell>powershell</shell><current_date>2000-01-01</current_date><timezone>UTC</timezone>\n</environment_context>";
        let mut body = json!({"input": [
            {"role":"user", "content":[{"type":"input_text", "text":context}]},
            {"role":"user", "content":[{"type":"input_text", "text":context}]},
            {"role":"user", "content":[{"type":"input_text", "text":format!("Explain this: {context}")}]},
            {"type":"function_call_output", "output":context}
        ]});
        let original = body.clone();
        assert!(rewrite_client_metadata_value(&mut body, &identity));
        assert_eq!(body["input"][0], original["input"][0]);
        assert_eq!(body["input"][2], original["input"][2]);
        assert_eq!(body["input"][3], original["input"][3]);
        let text = body["input"][1]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("<timezone>America/Los_Angeles</timezone>"));
        assert!(text.contains("<locale>en-US</locale>"));
        assert!(text.contains("<shell>powershell</shell>"));
        assert!(text.contains("<virtual_device>Mac OS"));
        assert!(!text.contains("2000-01-01"));
        let wire = serde_json::to_vec(&original).unwrap();
        let compressed = zstd::encode_all(wire.as_slice(), 3).unwrap();
        let rewritten =
            rewrite_client_metadata_in_body(&compressed, Some("zstd"), &identity).unwrap();
        let decoded: Value =
            serde_json::from_slice(&zstd::decode_all(rewritten.as_slice()).unwrap()).unwrap();
        assert_eq!(decoded, body);
    }

    #[test]
    fn environment_validates_and_persists_with_device() {
        let mut identity = VmIdentity::ephemeral();
        identity.environment.auto_region = false;
        identity.environment.timezone = "Asia/Tokyo".into();
        identity.environment.locale = "zh-CN".into();
        let reloaded: VmIdentity =
            serde_json::from_value(serde_json::to_value(&identity).unwrap()).unwrap();
        let reloaded = reloaded.with_runtime_ids();
        assert_eq!(reloaded.environment.timezone, "Asia/Tokyo");
        assert!(!reloaded.environment.auto_region);
        assert!(reloaded.environment.validate().is_ok());
        identity.environment.timezone = "Mars/Olympus".into();
        assert!(identity.environment.validate().is_err());
        identity.environment.timezone = "UTC".into();
        identity.environment.locale = "</locale>".into();
        assert!(identity.environment.validate().is_err());
    }

    #[test]
    fn canonical_metadata_and_compatibility_fields_agree() {
        let identity = VmIdentity::ephemeral();
        let mut body = json!({"client_metadata": {"installation_id":"old", "window_id":"old", "x-codex-turn-metadata":json!({"installation_id":"old","session_id":"old","window_id":"old","thread_id":"thread","turn_id":"turn"}).to_string()}});
        rewrite_client_metadata_value(&mut body, &identity);
        let snapshot: Value = serde_json::from_str(
            body["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot["installation_id"], identity.installation_id);
        assert_eq!(snapshot["session_id"], identity.session_id);
        assert_eq!(snapshot["thread_id"], "thread");
        assert_eq!(body["client_metadata"]["window_id"], identity.window_id);
        assert!(is_vm_identity_header("X-OpenAI-Subagent"));
    }

    #[test]
    fn user_agent_matches_the_codex_cli_shape() {
        let identity = VmIdentity::ephemeral();
        assert_eq!(
            identity.user_agent(),
            "codex_cli_rs/0.155.0 (Mac OS 15.5.0; arm64) xterm-256color"
        );
        assert_eq!(identity.routing_hint("gpt-6-astra"), "model=gpt-6-astra");
    }

    #[test]
    fn installation_id_survives_reload_and_session_does_not() {
        let path = std::env::temp_dir().join(format!("csk-vm-{}.json", Uuid::new_v4()));
        let first = VmIdentity::ephemeral();
        first.save_to(&path).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut second: VmIdentity = serde_json::from_str(&raw).unwrap();
        second.fill_runtime();
        assert_eq!(second.installation_id, first.installation_id);
        assert_ne!(second.session_id, first.session_id);
        assert_ne!(second.window_id, first.window_id);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn metadata_rewrite_keeps_thread_and_round_trips_zstd() {
        let identity = VmIdentity::ephemeral();
        let plain = serde_json::to_vec(&json!({
            "model": "m",
            "client_metadata": {
                "x-codex-installation-id": "client-install",
                "session_id": "client-session",
                "thread_id": "thread-1",
                "turn_id": "turn-1"
            }
        }))
        .unwrap();
        let compressed = zstd::encode_all(plain.as_slice(), 3).unwrap();
        let rewritten =
            rewrite_client_metadata_in_body(&compressed, Some("zstd"), &identity).unwrap();
        let decoded = zstd::decode_all(std::io::Cursor::new(rewritten)).unwrap();
        let value: Value = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(
            value["client_metadata"]["x-codex-installation-id"],
            json!(identity.installation_id)
        );
        assert_eq!(
            value["client_metadata"]["session_id"],
            json!(identity.session_id)
        );
        assert_eq!(value["client_metadata"]["thread_id"], json!("thread-1"));
        assert_eq!(value["client_metadata"]["turn_id"], json!("turn-1"));
        assert_eq!(value["model"], json!("m"));
    }

    #[test]
    fn body_without_metadata_is_unchanged() {
        let identity = VmIdentity::ephemeral();
        let bytes = br#"{"model":"m"}"#;
        let rewritten = rewrite_client_metadata_in_body(bytes, None, &identity).unwrap();
        assert_eq!(rewritten, bytes);
    }

    #[test]
    fn identity_headers_are_not_forwarded() {
        assert!(is_vm_identity_header("User-Agent"));
        assert!(is_vm_identity_header("x-codex-installation-id"));
        assert!(!is_vm_identity_header("authorization"));
        assert!(!is_vm_identity_header("x-codex-turn-state"));
    }

    #[test]
    fn platform_presets_set_every_system_field() {
        let mut identity = VmIdentity::ephemeral();
        identity.apply_profile(VmProfile {
            platform: DevicePlatform::Windows,
            environment: None,
        });
        assert_eq!(identity.platform(), DevicePlatform::Windows);
        assert_eq!(
            identity.user_agent(),
            "codex_cli_rs/0.155.0 (Windows 10.0.26100; x86_64) WindowsTerminal"
        );
        identity.apply_profile(VmProfile {
            platform: DevicePlatform::Linux,
            environment: None,
        });
        assert_eq!(
            identity.user_agent(),
            "codex_cli_rs/0.155.0 (Ubuntu 24.4.0; x86_64) xterm-256color"
        );
    }

    #[test]
    fn hand_edited_identities_are_brought_back_to_their_preset() {
        let mut identity = VmIdentity::ephemeral();
        identity.os_type = "Windows".into();
        identity.os_version = "15.5.0".into();
        identity.arch = "arm64".into();
        identity.originator = "custom".into();
        identity.cli_version = "latest".into();
        let installation = identity.installation_id.clone();
        let identity = identity.with_runtime_ids();
        assert_eq!(identity.installation_id, installation);
        assert_eq!(
            identity.user_agent(),
            "codex_cli_rs/0.155.0 (Windows 10.0.26100; x86_64) WindowsTerminal"
        );
        // Old files carrying the retired version lock still load.
        let raw = r#"{"installationId":"00000000-0000-4000-8000-000000000000","cliVersion":"0.160.0",
            "originator":"codex_cli_rs","osType":"Linux","osVersion":"6.8.0","arch":"x86_64",
            "terminal":"xterm-256color","versionLocked":true}"#;
        let old: VmIdentity = serde_json::from_str(raw).unwrap();
        let old = old.with_runtime_ids();
        assert_eq!(old.platform(), DevicePlatform::Linux);
        assert_eq!(old.os_version, "24.4.0");
        assert_eq!(old.cli_version, "0.160.0");
    }

    #[test]
    fn parses_codex_version_output() {
        assert_eq!(
            parse_cli_version("codex 0.160.0\n").as_deref(),
            Some("0.160.0")
        );
        assert_eq!(parse_cli_version("no version").as_deref(), None);
    }
}
