//! Kit 对外呈现的一台装着 Codex CLI 的机器。
//!
//! `installation_id` 写在磁盘上，重启后不变。根身份的 `session_id`、`window_id` 和 `thread_id`
//! 每次进程启动重新生成，作为请求身份的命名空间。并发窗口各自映射到稳定的会话/窗口身份，
//! 同账号继续共享设备身份。启用时探针、业务 HTTP 和上游 WebSocket 都用这一份设备身份；
//! `VmIdentity::enabled` 关闭时则跳过设备与环境改写，保留客户端原始信息。
//!
//! 系统只能在 Mac / Windows / Linux 三个预设里选。系统版本、架构和终端跟着预设走，
//! 取值与官方 CLI 在对应系统上用 `os_info` 和终端检测得到的一致，避免拼出不存在的组合
//! （例如 Windows 配 macOS 的版本号）。CLI 版本跟随本机安装的 codex，originator 固定。

use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::settings::{home_dir, is_dev_mode};

const DEFAULT_VERSION: &str = "0.155.0";
const ORIGINATOR: &str = "codex_cli_rs";
const INSTALLATION_ID_FILENAME: &str = ".codex-state-kit-installation_id";
const DEV_INSTALLATION_ID_FILENAME: &str = ".codex-state-kit-dev-installation_id";

fn enabled_by_default() -> bool {
    true
}

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
    "session-id",
    "session_id",
    "originator",
    "version",
    "user-agent",
    "thread-id",
    "x-client-request-id",
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmIdentity {
    /// Whether Kit replaces the client's device and environment metadata.
    /// Older identity files omit this field and keep the historical behavior.
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
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

/// User-controlled virtual device settings; platform and environment describe
/// the simulated identity when it is enabled.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmProfile {
    pub platform: DevicePlatform,
    pub environment: Option<VirtualEnvironment>,
    /// `None` keeps compatibility with older callers that only update the
    /// platform or environment.
    #[serde(default)]
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// Per-request conversation identifiers carried by the official Codex client.
///
/// Source session/window identifiers select a stable scope within the virtual
/// device, but their original values are never sent when rewriting is enabled.
/// Thread/turn identifiers describe the request's
/// conversation and must remain request-scoped so HTTP and WebSocket transports
/// expose the same routing identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RequestContext {
    pub session_id: Option<String>,
    pub window_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub parent_thread_id: Option<String>,
    pub subagent: Option<String>,
    pub turn_metadata: Option<String>,
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
    pub enabled: bool,
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
        let legacy_installation_id =
            valid_uuid(&identity.installation_id).then(|| identity.installation_id.clone());
        let installation_path = installation_id_path();
        identity.installation_id =
            resolve_installation_id(&installation_path, legacy_installation_id.as_deref())
                .unwrap_or_else(|_| Uuid::new_v4().to_string());
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

    /// Maps a client session/window into this device's runtime namespace.
    /// Call once on the root identity before rewriting request metadata. The
    /// deterministic mapping needs no shared mutable state, so overlapping
    /// tasks cannot replace one another's identifiers. The caller supplies a
    /// unique HTTP request scope or a stable WebSocket client connection scope
    /// for clients that send no usable conversation identifiers.
    pub fn scoped_for_request(&self, context: &RequestContext, fallback_scope: &str) -> Self {
        if !self.enabled {
            return self.clone();
        }
        let session_scope = context
            .session_id
            .as_deref()
            .map(|value| ("session", value))
            .or_else(|| context.window_id.as_deref().map(|value| ("window", value)))
            .or_else(|| context.thread_id.as_deref().map(|value| ("thread", value)))
            .unwrap_or(("fallback", fallback_scope));
        let window_scope = context
            .window_id
            .as_deref()
            .map(|value| ("window", value))
            .or_else(|| {
                context
                    .session_id
                    .as_deref()
                    .map(|value| ("session", value))
            })
            .or_else(|| context.thread_id.as_deref().map(|value| ("thread", value)))
            .unwrap_or(("fallback", fallback_scope));
        // Include both the installation and runtime IDs: accounts remain
        // isolated even if their clients use identical source identifiers,
        // and restarting/resetting the device starts a new mapping namespace.
        let namespace = Uuid::new_v5(
            &Uuid::NAMESPACE_OID,
            &serde_json::to_vec(&[
                "codex-state-kit/request-identity",
                &self.installation_id,
                &self.session_id,
                &self.window_id,
                &self.thread_id,
            ])
            .expect("serializing string arrays cannot fail"),
        );
        let derive = |kind: &str, source: (&str, &str)| {
            Uuid::new_v5(
                &namespace,
                &serde_json::to_vec(&[kind, source.0, source.1])
                    .expect("serializing string arrays cannot fail"),
            )
            .to_string()
        };
        let mut scoped = self.clone();
        scoped.session_id = derive("session", session_scope);
        scoped.window_id = derive("window", window_scope);
        scoped.thread_id = context
            .thread_id
            .clone()
            .unwrap_or_else(|| derive("thread", ("fallback", fallback_scope)));
        scoped
    }

    pub fn view(&self) -> VmIdentityView {
        VmIdentityView {
            enabled: self.enabled,
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
        if let Some(enabled) = profile.enabled {
            self.enabled = enabled;
        }
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
        // A device reset must not leave process-scoped identifiers that can be
        // correlated with the previous installation.
        self.fill_runtime();
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&identity_path())
    }

    fn fresh() -> Self {
        let mut identity = Self {
            enabled: true,
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
        // Codex uses UUIDv7 for request-scoped IDs so the values remain
        // sortable while retaining the UUID wire shape.
        self.session_id = Uuid::now_v7().to_string();
        self.window_id = Uuid::now_v7().to_string();
        self.thread_id = Uuid::now_v7().to_string();
    }

    fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).ok();
            }
        }
        let raw = serde_json::to_string_pretty(self).context("序列化虚拟设备身份")?;
        // Keep the legacy JSON field for older Kit versions, but make the
        // sidecar the canonical installation identity. The sidecar is locked
        // and fsynced before the profile JSON is replaced.
        let installation_path = installation_id_path_for(path);
        persist_installation_id(&installation_path, &self.installation_id)?;
        atomic_write(path, raw.as_bytes())?;
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

/// Canonical installation identity path. This mirrors Codex's standalone
/// `installation_id` file while keeping the Kit's dev and release profiles
/// isolated from one another.
pub fn installation_id_path() -> PathBuf {
    let name = if is_dev_mode() {
        DEV_INSTALLATION_ID_FILENAME
    } else {
        INSTALLATION_ID_FILENAME
    };
    home_dir().join(name)
}

fn installation_id_path_for(profile_path: &Path) -> PathBuf {
    if profile_path == identity_path() {
        return installation_id_path();
    }
    let file_name = profile_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("codex-state-kit-vm.json");
    profile_path.with_file_name(format!(".{file_name}.installation_id"))
}

/// Resolve the canonical installation ID while migrating an older profile's
/// JSON value when the sidecar does not exist yet. An existing invalid sidecar
/// is replaced, matching Codex's recovery behavior.
fn resolve_installation_id(path: &Path, legacy: Option<&str>) -> Result<String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 installation_id 目录 {}", parent.display()))?;
    }

    let mut file = open_locked_installation_file(path)?;
    file.lock_exclusive()
        .with_context(|| format!("锁定 installation_id {}", path.display()))?;

    let result = (|| {
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .with_context(|| format!("读取 installation_id {}", path.display()))?;
        let trimmed = contents.trim();
        if !trimmed.is_empty() {
            if let Ok(existing) = Uuid::parse_str(trimmed) {
                return Ok(existing.to_string());
            }
        } else if let Some(legacy) = legacy.and_then(parse_uuid) {
            return write_installation_id(&mut file, path, &legacy);
        }

        let installation_id = Uuid::new_v4().to_string();
        write_installation_id(&mut file, path, &installation_id)
    })();

    let unlock_result = file.unlock();
    match (result, unlock_result) {
        (Err(error), _) => Err(error),
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(anyhow::anyhow!(
            "解锁 installation_id {}: {error}",
            path.display()
        )),
    }
}

fn parse_uuid(value: &str) -> Option<String> {
    Uuid::parse_str(value.trim())
        .ok()
        .map(|uuid| uuid.to_string())
}

fn open_locked_installation_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let file = options
        .open(path)
        .with_context(|| format!("打开 installation_id {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = file
            .metadata()
            .with_context(|| format!("读取 installation_id 权限 {}", path.display()))?;
        let current_mode = metadata.permissions().mode() & 0o777;
        if current_mode != 0o644 {
            let mut permissions = metadata.permissions();
            permissions.set_mode(0o644);
            file.set_permissions(permissions)
                .with_context(|| format!("修正 installation_id 权限 {}", path.display()))?;
        }
    }
    Ok(file)
}

fn write_installation_id(file: &mut File, path: &Path, installation_id: &str) -> Result<String> {
    file.set_len(0)
        .with_context(|| format!("截断 installation_id {}", path.display()))?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("定位 installation_id {}", path.display()))?;
    file.write_all(installation_id.as_bytes())
        .with_context(|| format!("写入 installation_id {}", path.display()))?;
    file.flush()
        .with_context(|| format!("刷新 installation_id {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("同步 installation_id {}", path.display()))?;
    Ok(installation_id.to_owned())
}

fn persist_installation_id(path: &Path, installation_id: &str) -> Result<()> {
    let installation_id = parse_uuid(installation_id)
        .ok_or_else(|| anyhow::anyhow!("installation_id 不是有效 UUID"))?;
    let mut file = open_locked_installation_file(path)?;
    file.lock_exclusive()
        .with_context(|| format!("锁定 installation_id {}", path.display()))?;
    let result = write_installation_id(&mut file, path, &installation_id);
    let unlock_result = file.unlock();
    match (result, unlock_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Ok(())) => Ok(()),
        (Ok(_), Err(error)) => Err(anyhow::anyhow!(
            "解锁 installation_id {}: {error}",
            path.display()
        )),
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("identity.json");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let write_result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("创建临时身份文件 {}", temporary.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("写入临时身份文件 {}", temporary.display()))?;
        file.flush()
            .with_context(|| format!("刷新临时身份文件 {}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("同步临时身份文件 {}", temporary.display()))?;
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(_first_error) if path.exists() => {
            // Windows does not replace an existing destination with rename.
            // Move the old file aside, install the fsynced temporary file, and
            // restore the old file if the second rename fails.
            let backup = parent.join(format!(".{file_name}.{}.bak", Uuid::new_v4()));
            if let Err(error) = std::fs::rename(path, &backup) {
                let _ = std::fs::remove_file(&temporary);
                return Err(error).with_context(|| format!("备份身份文件 {}", path.display()));
            }
            match std::fs::rename(&temporary, path) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&backup);
                    Ok(())
                }
                Err(error) => {
                    let _ = std::fs::rename(&backup, path);
                    let _ = std::fs::remove_file(&temporary);
                    Err(error).with_context(|| format!("替换身份文件 {}", path.display()))
                }
            }
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error).with_context(|| format!("替换身份文件 {}", path.display()))
        }
    }
}

pub fn is_vm_identity_header(name: &str) -> bool {
    IDENTITY_HEADERS
        .iter()
        .any(|header| header.eq_ignore_ascii_case(name))
}

fn safe_metadata_string(value: Option<&Value>) -> Option<String> {
    let value = value.and_then(Value::as_str)?.trim();
    if value.is_empty() || !value.is_ascii() || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_owned())
}

fn ascii_json(value: &Value) -> String {
    let raw = value.to_string();
    let mut escaped = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii() {
            escaped.push(ch);
            continue;
        }
        let code = ch as u32;
        if code <= 0xffff {
            let _ = write!(escaped, "\\u{code:04x}");
        } else {
            let code = code - 0x1_0000;
            let high = 0xd800 + (code >> 10);
            let low = 0xdc00 + (code & 0x3ff);
            let _ = write!(escaped, "\\u{high:04x}\\u{low:04x}");
        }
    }
    escaped
}

fn valid_turn_metadata(value: Option<&Value>) -> Option<String> {
    let value = value.and_then(Value::as_str)?.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return None;
    }
    let parsed = serde_json::from_str::<Value>(&value).ok()?;
    matches!(parsed, Value::Object(_)).then(|| ascii_json(&parsed))
}

/// Reads request-scoped metadata without trusting it for the virtual device.
/// Flat keys are the compatibility projection used by Codex; the canonical
/// turn metadata JSON fills fields that an older client omitted from that
/// projection.
pub fn request_context_from_value(body: &Value) -> RequestContext {
    let metadata = body.get("client_metadata").and_then(Value::as_object);
    let mut context = RequestContext {
        session_id: metadata.and_then(|m| safe_metadata_string(m.get("session_id"))),
        window_id: metadata.and_then(|m| {
            safe_metadata_string(m.get("x-codex-window-id"))
                .or_else(|| safe_metadata_string(m.get("window_id")))
        }),
        thread_id: metadata.and_then(|m| safe_metadata_string(m.get("thread_id"))),
        turn_id: metadata.and_then(|m| safe_metadata_string(m.get("turn_id"))),
        parent_thread_id: metadata
            .and_then(|m| safe_metadata_string(m.get("x-codex-parent-thread-id"))),
        subagent: metadata.and_then(|m| safe_metadata_string(m.get("x-openai-subagent"))),
        turn_metadata: metadata.and_then(|m| valid_turn_metadata(m.get("x-codex-turn-metadata"))),
    };
    if let Some(raw) = context.turn_metadata.as_deref() {
        if let Ok(Value::Object(snapshot)) = serde_json::from_str::<Value>(raw) {
            if context.session_id.is_none() {
                context.session_id = safe_metadata_string(snapshot.get("session_id"));
            }
            if context.window_id.is_none() {
                context.window_id = safe_metadata_string(snapshot.get("window_id"));
            }
            if context.thread_id.is_none() {
                context.thread_id = safe_metadata_string(snapshot.get("thread_id"));
            }
            if context.turn_id.is_none() {
                context.turn_id = safe_metadata_string(snapshot.get("turn_id"));
            }
            if context.parent_thread_id.is_none() {
                context.parent_thread_id = safe_metadata_string(snapshot.get("parent_thread_id"));
            }
            if context.subagent.is_none() {
                context.subagent = safe_metadata_string(snapshot.get("subagent_kind"));
            }
        }
    }
    context
}

/// Best-effort extraction for a compressed Responses request.
pub fn request_context_from_body(bytes: &[u8], encoding: Option<&str>) -> Option<RequestContext> {
    let (plain, _) = decode_body(bytes, encoding).ok()?;
    let value = serde_json::from_slice::<Value>(&plain).ok()?;
    Some(request_context_from_value(&value))
}

fn is_responses_request(body: &Value) -> bool {
    body.get("model").and_then(Value::as_str).is_some()
        && (body.get("input").is_some()
            || body.get("type").and_then(Value::as_str) == Some("response.create"))
}

fn update_turn_metadata_snapshot(
    metadata: &mut serde_json::Map<String, Value>,
    identity: &VmIdentity,
    context: &RequestContext,
) {
    let Some(raw) = metadata
        .get("x-codex-turn-metadata")
        .and_then(Value::as_str)
    else {
        return;
    };
    let Ok(Value::Object(mut snapshot)) = serde_json::from_str::<Value>(raw) else {
        return;
    };
    snapshot.insert("installation_id".into(), json!(identity.installation_id));
    snapshot.insert("session_id".into(), json!(identity.session_id));
    snapshot.insert("window_id".into(), json!(identity.window_id));
    snapshot.insert(
        "thread_id".into(),
        json!(context
            .thread_id
            .as_deref()
            .unwrap_or(identity.thread_id.as_str())),
    );
    if let Some(turn_id) = context.turn_id.as_deref() {
        snapshot.insert("turn_id".into(), json!(turn_id));
    }
    if let Some(parent_thread_id) = context.parent_thread_id.as_deref() {
        snapshot.insert("parent_thread_id".into(), json!(parent_thread_id));
    }
    if let Some(subagent) = context.subagent.as_deref() {
        snapshot.insert("subagent_kind".into(), json!(subagent));
    }
    metadata.insert(
        "x-codex-turn-metadata".into(),
        Value::String(ascii_json(&Value::Object(snapshot))),
    );
}

/// Only a standalone harness environment message is eligible. Never rewrite
/// instructions, user prose, tool output, paths or the real execution shell.
fn rewrite_environment(body: &mut Value, identity: &VmIdentity) -> bool {
    if !identity.enabled {
        return false;
    }
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
    rewrite_client_metadata_value_with_context(body, identity, None)
}

/// Rewrites the request metadata while supplying identifiers observed on the
/// HTTP/WebSocket handshake when the client omitted `client_metadata` from the
/// body. This keeps the body and compatibility headers in agreement.
pub fn rewrite_client_metadata_value_with_context(
    body: &mut Value,
    identity: &VmIdentity,
    request_context: Option<&RequestContext>,
) -> bool {
    if !identity.enabled {
        return false;
    }
    let environment_changed = rewrite_environment(body, identity);
    let mut context = request_context_from_value(body);
    if let Some(request_context) = request_context {
        if context.thread_id.is_none() {
            context.thread_id = request_context.thread_id.clone();
        }
        if context.turn_id.is_none() {
            context.turn_id = request_context.turn_id.clone();
        }
        if context.parent_thread_id.is_none() {
            context.parent_thread_id = request_context.parent_thread_id.clone();
        }
        if context.subagent.is_none() {
            context.subagent = request_context.subagent.clone();
        }
        if context.turn_metadata.is_none() {
            context.turn_metadata = request_context.turn_metadata.clone();
        }
    }
    let response_request = is_responses_request(body);
    let has_identity_metadata = body
        .get("client_metadata")
        .and_then(Value::as_object)
        .is_some_and(|metadata| {
            metadata.keys().any(|key| {
                matches!(
                    key.as_str(),
                    "x-codex-installation-id"
                        | "installation_id"
                        | "session_id"
                        | "thread_id"
                        | "x-codex-window-id"
                        | "window_id"
                        | "x-codex-turn-metadata"
                )
            })
        });
    if !response_request && !has_identity_metadata {
        return environment_changed;
    }
    if body.get("client_metadata").is_none() {
        body["client_metadata"] = Value::Object(serde_json::Map::new());
    }
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
    metadata.insert(
        "thread_id".into(),
        json!(context
            .thread_id
            .as_deref()
            .unwrap_or(identity.thread_id.as_str())),
    );
    if let Some(turn_id) = context.turn_id.as_deref() {
        metadata.insert("turn_id".into(), json!(turn_id));
    }
    if let Some(parent_thread_id) = context.parent_thread_id.as_deref() {
        metadata.insert("x-codex-parent-thread-id".into(), json!(parent_thread_id));
    }
    if let Some(subagent) = context.subagent.as_deref() {
        metadata.insert("x-openai-subagent".into(), json!(subagent));
    }
    if !metadata.contains_key("x-codex-turn-metadata") {
        if let Some(turn_metadata) = context.turn_metadata.as_deref() {
            metadata.insert("x-codex-turn-metadata".into(), json!(turn_metadata));
        }
    }
    for (key, value) in [
        ("installation_id", &identity.installation_id),
        ("window_id", &identity.window_id),
    ] {
        if metadata.contains_key(key) {
            metadata.insert(key.into(), json!(value));
        }
    }
    // Newer core versions carry the authoritative snapshot as a JSON string.
    update_turn_metadata_snapshot(metadata, identity, &context);
    true
}

/// 解压 JSON 正文，替换 `client_metadata` 里的设备字段，再按原编码写回。
/// 同时改写最近的独立 environment_context。没有适用字段时保留原正文。
pub fn rewrite_client_metadata_in_body(
    bytes: &[u8],
    encoding: Option<&str>,
    identity: &VmIdentity,
) -> Result<Vec<u8>, String> {
    rewrite_client_metadata_in_body_with_context(bytes, encoding, identity, None)
}

pub fn rewrite_client_metadata_in_body_with_context(
    bytes: &[u8],
    encoding: Option<&str>,
    identity: &VmIdentity,
    request_context: Option<&RequestContext>,
) -> Result<Vec<u8>, String> {
    let (plain, codec) = decode_body(bytes, encoding)?;
    let mut value: Value = match serde_json::from_slice(&plain) {
        Ok(value) => value,
        Err(_) => return Ok(bytes.to_vec()),
    };
    if !rewrite_client_metadata_value_with_context(&mut value, identity, request_context) {
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
    for token in text.split_whitespace() {
        // `codex --version` has been emitted as both `codex 0.155.0` and
        // `codex v0.155.0-beta.1+build`. Keep the complete SemVer token so
        // the User-Agent does not silently downgrade a prerelease build to
        // its base version.
        let token = token
            .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && !matches!(ch, '.' | '-' | '+'));
        let token = token.strip_prefix(['v', 'V']).unwrap_or(token);
        if is_version(token) {
            return Some(token.to_string());
        }
    }
    None
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
    semver::Version::parse(value).is_ok()
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
        assert!(is_vm_identity_header("Session-Id"));
    }

    #[test]
    fn request_context_reads_session_and_window_from_flat_and_snapshot_metadata() {
        let body = json!({
            "client_metadata": {
                "session_id": "flat-session",
                "x-codex-window-id": "flat-window",
                "x-codex-turn-metadata": r#"{"session_id":"snapshot-session","window_id":"snapshot-window","thread_id":"snapshot-thread"}"#
            }
        });
        let context = request_context_from_value(&body);
        assert_eq!(context.session_id.as_deref(), Some("flat-session"));
        assert_eq!(context.window_id.as_deref(), Some("flat-window"));
        assert_eq!(context.thread_id.as_deref(), Some("snapshot-thread"));

        let snapshot_only = json!({
            "client_metadata": {
                "x-codex-turn-metadata": r#"{"session_id":"snapshot-session","window_id":"snapshot-window"}"#
            }
        });
        let context = request_context_from_value(&snapshot_only);
        assert_eq!(context.session_id.as_deref(), Some("snapshot-session"));
        assert_eq!(context.window_id.as_deref(), Some("snapshot-window"));
    }

    #[test]
    fn scoped_identity_is_stable_and_separates_windows_and_accounts() {
        let root = VmIdentity::ephemeral();
        let first = RequestContext {
            session_id: Some("session-a".into()),
            window_id: Some("window-a".into()),
            thread_id: Some("thread-a".into()),
            ..RequestContext::default()
        };
        let second_window = RequestContext {
            session_id: Some("session-a".into()),
            window_id: Some("window-b".into()),
            thread_id: Some("thread-b".into()),
            ..RequestContext::default()
        };
        let first_mapping = root.scoped_for_request(&first, "http-1");
        let first_again = root.scoped_for_request(&first, "http-2");
        let second_mapping = root.scoped_for_request(&second_window, "http-3");
        assert_eq!(first_mapping.session_id, first_again.session_id);
        assert_eq!(first_mapping.window_id, first_again.window_id);
        assert_eq!(first_mapping.thread_id, "thread-a");
        assert_ne!(first_mapping.window_id, second_mapping.window_id);
        assert_eq!(first_mapping.session_id, second_mapping.session_id);

        let concurrent: Vec<_> = (0..8)
            .map(|_| {
                let identity = root.clone();
                let context = first.clone();
                std::thread::spawn(move || identity.scoped_for_request(&context, "http-race"))
            })
            .collect();
        for worker in concurrent {
            let mapping = worker.join().unwrap();
            assert_eq!(mapping.session_id, first_mapping.session_id);
            assert_eq!(mapping.window_id, first_mapping.window_id);
        }

        let other_account = root.renewed();
        let other_mapping = other_account.scoped_for_request(&first, "http-1");
        assert_ne!(first_mapping.session_id, other_mapping.session_id);
        assert_ne!(first_mapping.window_id, other_mapping.window_id);
    }

    #[test]
    fn scoped_identity_uses_fallback_for_metadata_free_requests() {
        let root = VmIdentity::ephemeral();
        let context = RequestContext::default();
        let first = root.scoped_for_request(&context, "socket-a");
        let same = root.scoped_for_request(&context, "socket-a");
        let second = root.scoped_for_request(&context, "socket-b");
        assert_eq!(first.session_id, same.session_id);
        assert_eq!(first.window_id, same.window_id);
        assert_ne!(first.session_id, second.session_id);
        assert_ne!(first.window_id, second.window_id);
        assert_ne!(first.thread_id, root.thread_id);
    }

    #[test]
    fn disabled_scoping_preserves_the_client_identity_path() {
        let mut root = VmIdentity::ephemeral();
        root.enabled = false;
        let context = RequestContext {
            session_id: Some("client-session".into()),
            window_id: Some("client-window".into()),
            thread_id: Some("client-thread".into()),
            ..RequestContext::default()
        };
        assert_eq!(root.scoped_for_request(&context, "http-1"), root);
    }

    #[test]
    fn canonical_turn_metadata_is_ascii_safe_for_headers() {
        let identity = VmIdentity::ephemeral();
        let mut body = json!({
            "model": "m",
            "input": [],
            "client_metadata": {
                "x-codex-turn-metadata": r#"{"label":"日本語"}"#
            }
        });
        assert!(rewrite_client_metadata_value(&mut body, &identity));
        let encoded = body["client_metadata"]["x-codex-turn-metadata"]
            .as_str()
            .unwrap();
        assert!(encoded.is_ascii());
        assert!(encoded.contains(r#"\u65e5"#));
    }

    #[test]
    fn responses_without_metadata_receive_the_official_identity_baseline() {
        let identity = VmIdentity::ephemeral();
        let mut body = json!({"model":"gpt-test","input":[]});
        assert!(rewrite_client_metadata_value(&mut body, &identity));
        let metadata = body["client_metadata"].as_object().unwrap();
        assert_eq!(
            metadata["x-codex-installation-id"],
            identity.installation_id
        );
        assert_eq!(metadata["session_id"], identity.session_id);
        assert_eq!(metadata["x-codex-window-id"], identity.window_id);
        assert_eq!(metadata["thread_id"], identity.thread_id);
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
    fn installation_sidecar_migrates_legacy_and_rewrites_invalid_values() {
        let directory = tempfile::tempdir().unwrap();
        let sidecar = directory.path().join("installation_id");
        let legacy = Uuid::new_v4().to_string();
        let resolved = resolve_installation_id(&sidecar, Some(&legacy)).unwrap();
        assert_eq!(resolved, legacy);
        assert_eq!(std::fs::read_to_string(&sidecar).unwrap(), legacy);

        let other = Uuid::new_v4().to_string();
        assert_eq!(
            resolve_installation_id(&sidecar, Some(&other)).unwrap(),
            legacy
        );

        std::fs::write(&sidecar, "not-a-uuid").unwrap();
        let regenerated = resolve_installation_id(&sidecar, Some(&other)).unwrap();
        assert_ne!(regenerated, legacy);
        assert_ne!(regenerated, other);
        assert_eq!(parse_uuid(&regenerated), Some(regenerated.clone()));
    }

    #[test]
    fn atomic_profile_write_replaces_an_existing_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("profile.json");
        atomic_write(&path, br#"{"version":1}"#).unwrap();
        atomic_write(&path, br#"{"version":2}"#).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), r#"{"version":2}"#);
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
    fn disabled_identity_leaves_client_metadata_unchanged() {
        let mut identity = VmIdentity::ephemeral();
        identity.enabled = false;
        let mut body = json!({
            "model": "gpt-test",
            "input": [],
            "client_metadata": {
                "x-codex-installation-id": "client-install",
                "session_id": "client-session",
                "thread_id": "client-thread"
            }
        });
        let before = body.clone();
        assert!(!rewrite_client_metadata_value(&mut body, &identity));
        assert_eq!(body, before);
    }

    #[test]
    fn disabled_identity_leaves_device_and_environment_untouched() {
        let mut identity = VmIdentity::ephemeral();
        identity.enabled = false;
        let value = json!({
            "model": "m",
            "client_metadata": {
                "x-codex-installation-id": "client-install",
                "session_id": "client-session",
                "x-codex-window-id": "client-window"
            },
            "input": [{
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "<environment_context><virtual_device>client-device</virtual_device><timezone>UTC</timezone></environment_context>"
                }]
            }]
        });
        let mut rewritten = value.clone();
        assert!(!rewrite_client_metadata_value(&mut rewritten, &identity));
        assert_eq!(rewritten, value);

        let bytes = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            rewrite_client_metadata_in_body(&bytes, None, &identity).unwrap(),
            bytes
        );
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
            enabled: None,
        });
        assert_eq!(identity.platform(), DevicePlatform::Windows);
        assert_eq!(
            identity.user_agent(),
            "codex_cli_rs/0.155.0 (Windows 10.0.26100; x86_64) WindowsTerminal"
        );
        identity.apply_profile(VmProfile {
            platform: DevicePlatform::Linux,
            environment: None,
            enabled: None,
        });
        assert_eq!(
            identity.user_agent(),
            "codex_cli_rs/0.155.0 (Ubuntu 24.4.0; x86_64) xterm-256color"
        );
        identity.apply_profile(VmProfile {
            platform: DevicePlatform::Linux,
            environment: None,
            enabled: Some(false),
        });
        assert!(!identity.enabled);
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
        assert!(old.enabled);
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
        assert_eq!(
            parse_cli_version("codex v0.161.0-beta.2+nightly\n").as_deref(),
            Some("0.161.0-beta.2+nightly")
        );
        assert_eq!(
            parse_cli_version("Codex CLI (v0.162.0)\n").as_deref(),
            Some("0.162.0")
        );
        assert_eq!(parse_cli_version("no version").as_deref(), None);
        assert_eq!(parse_cli_version("codex 0.162\n").as_deref(), None);
    }
}
