//! Kit 对外呈现的一台 Mac 上的 Codex CLI。
//!
//! `installation_id` 写在磁盘上，重启后不变。`session_id`、`window_id` 和 `thread_id`
//! 每次进程启动重新生成。探针、业务 HTTP 和上游 WebSocket 都用这一份，不透传客户端自己的设备头。

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::settings::{home_dir, is_dev_mode};

const DEFAULT_VERSION: &str = "0.155.0";
const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";
const DEFAULT_OS_TYPE: &str = "Mac OS";
const DEFAULT_OS_VERSION: &str = "15.5.0";
const DEFAULT_ARCH: &str = "arm64";
const DEFAULT_TERMINAL: &str = "xterm-256color";

const IDENTITY_HEADERS: &[&str] = &[
    "x-codex-installation-id",
    "x-codex-routing-hint",
    "x-codex-window-id",
    "x-codex-turn-metadata",
    "x-codex-parent-thread-id",
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
    pub installation_id: String,
    pub cli_version: String,
    pub originator: String,
    pub os_type: String,
    pub os_version: String,
    pub arch: String,
    pub terminal: String,
    #[serde(default)]
    pub version_locked: bool,
    #[serde(skip)]
    pub session_id: String,
    #[serde(skip)]
    pub window_id: String,
    #[serde(skip)]
    pub thread_id: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmProfile {
    pub cli_version: String,
    pub originator: String,
    pub os_type: String,
    pub os_version: String,
    pub arch: String,
    pub terminal: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VmIdentityView {
    pub installation_id: String,
    pub session_id: String,
    pub cli_version: String,
    pub originator: String,
    pub os_type: String,
    pub os_version: String,
    pub arch: String,
    pub terminal: String,
    pub user_agent: String,
    pub version_locked: bool,
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
        identity.fill_runtime();
        #[cfg(not(test))]
        if !identity.version_locked {
            if let Some(version) = detect_local_cli_version() {
                identity.cli_version = version;
            }
        }
        if identity.save_to(&path).is_err() {
            eprintln!("[identity] 无法写入 {}", path.display());
        }
        identity
    }

    /// 测试和不落盘的临时身份。字段与默认 Mac CLI 指纹一致。
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
            installation_id: self.installation_id.clone(),
            session_id: self.session_id.clone(),
            cli_version: self.cli_version.clone(),
            originator: self.originator.clone(),
            os_type: self.os_type.clone(),
            os_version: self.os_version.clone(),
            arch: self.arch.clone(),
            terminal: self.terminal.clone(),
            user_agent: self.user_agent(),
            version_locked: self.version_locked,
        }
    }

    pub fn apply_profile(&mut self, profile: VmProfile) -> Result<()> {
        let next = Self {
            installation_id: self.installation_id.clone(),
            cli_version: normalize_version(&profile.cli_version)?,
            originator: normalize_token(&profile.originator, "originator", 64)?,
            os_type: normalize_os_type(&profile.os_type)?,
            os_version: normalize_token(&profile.os_version, "系统版本", 32)?,
            arch: normalize_arch(&profile.arch)?,
            terminal: normalize_token(&profile.terminal, "终端", 64)?,
            version_locked: true,
            session_id: self.session_id.clone(),
            window_id: self.window_id.clone(),
            thread_id: self.thread_id.clone(),
        };
        *self = next;
        Ok(())
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
        Self {
            installation_id: Uuid::new_v4().to_string(),
            cli_version: DEFAULT_VERSION.into(),
            originator: DEFAULT_ORIGINATOR.into(),
            os_type: DEFAULT_OS_TYPE.into(),
            os_version: DEFAULT_OS_VERSION.into(),
            arch: DEFAULT_ARCH.into(),
            terminal: DEFAULT_TERMINAL.into(),
            version_locked: false,
            session_id: String::new(),
            window_id: String::new(),
            thread_id: String::new(),
        }
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

pub fn rewrite_client_metadata_value(body: &mut Value, identity: &VmIdentity) -> bool {
    let Some(metadata) = body
        .get_mut("client_metadata")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    metadata.insert(
        "x-codex-installation-id".into(),
        json!(identity.installation_id),
    );
    metadata.insert("session_id".into(), json!(identity.session_id));
    metadata.insert("x-codex-window-id".into(), json!(identity.window_id));
    true
}

/// 解压 JSON 正文，替换 `client_metadata` 里的设备字段，再按原编码写回。
/// 没有 `client_metadata` 时原样返回。解析失败时返回错误，调用方保留原正文。
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

fn normalize_version(raw: &str) -> Result<String> {
    let value = raw.trim();
    if !is_version(value) {
        bail!("CLI 版本需要是 x.y.z");
    }
    Ok(value.to_string())
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

fn normalize_os_type(raw: &str) -> Result<String> {
    match raw.trim() {
        "Mac OS" => Ok("Mac OS".into()),
        "Linux" => Ok("Linux".into()),
        "Windows" => Ok("Windows".into()),
        _ => bail!("系统类型只能是 Mac OS、Linux 或 Windows"),
    }
}

fn normalize_arch(raw: &str) -> Result<String> {
    match raw.trim() {
        "arm64" => Ok("arm64".into()),
        "x86_64" => Ok("x86_64".into()),
        _ => bail!("架构只能是 arm64 或 x86_64"),
    }
}

fn normalize_token(raw: &str, label: &str, max: usize) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() || value.len() > max || value.chars().any(|ch| ch.is_control() || ch == ' ')
    {
        bail!("{label}无效");
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | ':' | '-'))
    {
        bail!("{label}包含不支持的字符");
    }
    Ok(value.to_string())
}

fn valid_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn profile_rejects_a_bad_version_and_locks_manual_edits() {
        let mut identity = VmIdentity::ephemeral();
        let err = identity
            .apply_profile(VmProfile {
                cli_version: "latest".into(),
                originator: "codex_cli_rs".into(),
                os_type: "Mac OS".into(),
                os_version: "15.5.0".into(),
                arch: "arm64".into(),
                terminal: "xterm-256color".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("x.y.z"));
        identity
            .apply_profile(VmProfile {
                cli_version: "0.160.0".into(),
                originator: "codex_cli_rs".into(),
                os_type: "Linux".into(),
                os_version: "6.8.0".into(),
                arch: "x86_64".into(),
                terminal: "xterm-256color".into(),
            })
            .unwrap();
        assert!(identity.version_locked);
        assert_eq!(
            identity.user_agent(),
            "codex_cli_rs/0.160.0 (Linux 6.8.0; x86_64) xterm-256color"
        );
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
