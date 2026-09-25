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
use sha2::{Digest, Sha256};
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
    /// Terminal name/token used in the Codex user agent. This remains named
    /// `terminal` for compatibility with profiles written by older Kit
    /// versions; the optional version and multiplexer fields below carry the
    /// additional runtime terminal fingerprint detected by Codex.
    pub terminal: String,
    #[serde(default)]
    pub terminal_version: String,
    #[serde(default)]
    pub terminal_multiplexer: String,
    /// A profile may explicitly choose a terminal. Explicit values must not
    /// be replaced by runtime detection when the profile is loaded again.
    #[serde(default, skip_serializing_if = "is_false")]
    terminal_override: bool,
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
    /// Optional terminal override. Omitting these fields keeps the platform
    /// preset and allows runtime terminal detection on the next load.
    #[serde(default)]
    pub terminal: Option<String>,
    #[serde(default)]
    pub terminal_version: Option<String>,
    #[serde(default)]
    pub terminal_multiplexer: Option<String>,
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
    pub parent_turn_id: Option<String>,
    pub root_turn_id: Option<String>,
    pub forked_from_thread_id: Option<String>,
    pub context_window_id: Option<String>,
    pub window_number: Option<u64>,
    pub agent_name: Option<String>,
    pub thread_source: Option<String>,
    pub turn_trigger: Option<String>,
    pub request_kind: Option<String>,
    pub sandbox: Option<String>,
    pub sandbox_mode: Option<String>,
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
    pub terminal_version: String,
    pub terminal_multiplexer: String,
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
        if !identity.terminal_override {
            identity.detect_runtime_terminal();
        }
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
        let terminal = if self.terminal_version.trim().is_empty() {
            self.terminal.clone()
        } else {
            format!("{}/{}", self.terminal, self.terminal_version)
        };
        let base = format!(
            "{}/{} ({} {}; {}) {}",
            self.originator_value(),
            self.cli_version,
            self.os_type,
            self.os_version,
            self.arch,
            terminal
        );
        match user_agent_suffix() {
            Some(suffix) => format!("{base} ({suffix})"),
            None => base,
        }
    }

    /// The official client permits a process-level originator override. Keep
    /// the persisted profile as the default while honoring that override for
    /// every request path that shares this identity.
    pub fn originator_value(&self) -> String {
        std::env::var("CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
            .ok()
            .and_then(|value| {
                let value = value.trim();
                (!value.is_empty()
                    && value.len() <= 128
                    && value.is_ascii()
                    && !value.chars().any(char::is_control))
                .then(|| value.to_owned())
            })
            .unwrap_or_else(|| self.originator.clone())
    }

    /// Builds the request User-Agent without loading or persisting the
    /// virtual-device profile.  Login and auxiliary clients use this helper
    /// so they expose the same runtime terminal metadata as the proxy while
    /// keeping authentication side-effect free.
    pub(crate) fn runtime_user_agent() -> String {
        let mut identity = Self::ephemeral();
        identity.detect_runtime_terminal();
        #[cfg(not(test))]
        if let Some(version) = detect_local_cli_version() {
            identity.cli_version = version;
        }
        identity.user_agent()
    }

    pub fn routing_hint(&self, model: &str) -> String {
        self.routing_hint_with_tier(model, None)
    }

    pub fn routing_hint_with_tier(&self, model: &str, service_tier: Option<&str>) -> String {
        let model = model.trim();
        match service_tier.map(str::trim).filter(|tier| {
            !tier.is_empty()
                && tier.len() <= 64
                && tier.is_ascii()
                && !tier.chars().any(char::is_control)
        }) {
            Some(tier) => format!("model={model};tier={tier}"),
            None => format!("model={model}"),
        }
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
        let window_scope = context
            .window_id
            .as_deref()
            .map(|value| ("window", value.to_owned()))
            .or_else(|| {
                context
                    .session_id
                    .as_deref()
                    .map(|value| ("session", value.to_owned()))
            })
            .or_else(|| {
                context
                    .thread_id
                    .as_deref()
                    .map(|value| ("thread", value.to_owned()))
            })
            .unwrap_or(("fallback", fallback_scope.to_owned()));
        let thread_scope = context
            .thread_id
            .as_deref()
            .map(|value| ("thread", value.to_owned()))
            .unwrap_or(("fallback", fallback_scope.to_owned()));
        let mut scoped = self.clone();
        // Codex keeps one root session across windows and uses UUIDv7 for all
        // generated protocol identifiers.  Keep the process/root session and
        // derive stable UUIDv7-shaped IDs for client-provided scopes so the
        // same source window/thread maps consistently across HTTP and WS.
        scoped.session_id = self.session_id.clone();
        scoped.window_id = self.scoped_uuid_v7("window", &window_scope.0, &window_scope.1);
        scoped.thread_id = self.scoped_uuid_v7("thread", &thread_scope.0, &thread_scope.1);
        scoped
    }

    fn scoped_uuid_v7(&self, kind: &str, scope_kind: &str, source: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(b"codex-state-kit/request-identity/v7\0");
        hasher.update(self.installation_id.as_bytes());
        hasher.update(self.session_id.as_bytes());
        hasher.update(kind.as_bytes());
        hasher.update(b"\0");
        hasher.update(scope_kind.as_bytes());
        hasher.update(b"\0");
        hasher.update(source.as_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(&digest[..16]);
        // Preserve the root UUIDv7 timestamp when available, while using the
        // digest for the remaining stable random bits. This yields a stable,
        // sortable UUIDv7 family instead of exposing UUIDv5 version bits.
        if let Ok(root) = Uuid::parse_str(&self.session_id) {
            bytes[..6].copy_from_slice(&root.as_bytes()[..6]);
        }
        bytes[6] = (bytes[6] & 0x0f) | 0x70;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Uuid::from_bytes(bytes).to_string()
    }

    /// Maps a client-owned protocol identifier into this virtual device's
    /// UUIDv7-shaped namespace.  The mapping is stable for the same source,
    /// which keeps HTTP and WebSocket turns correlated without exposing the
    /// client's UUID version or account lineage.
    pub fn map_protocol_id(&self, kind: &str, source: &str) -> String {
        self.scoped_uuid_v7(kind, kind, source)
    }

    pub fn view(&self) -> VmIdentityView {
        VmIdentityView {
            enabled: self.enabled,
            environment: self.environment.clone(),
            installation_id: self.installation_id.clone(),
            session_id: self.session_id.clone(),
            platform: self.platform(),
            cli_version: self.cli_version.clone(),
            originator: self.originator_value(),
            os_type: self.os_type.clone(),
            os_version: self.os_version.clone(),
            arch: self.arch.clone(),
            terminal: self.terminal.clone(),
            terminal_version: self.terminal_version.clone(),
            terminal_multiplexer: self.terminal_multiplexer.clone(),
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
        if let Some(terminal) = profile.terminal {
            self.terminal = sanitize_terminal_token(&terminal);
            self.terminal_override = !self.terminal.is_empty();
        }
        if let Some(version) = profile.terminal_version {
            self.terminal_version = sanitize_terminal_token(&version);
            self.terminal_override = true;
        }
        if let Some(multiplexer) = profile.terminal_multiplexer {
            self.terminal_multiplexer = sanitize_terminal_token(&multiplexer);
            self.terminal_override = true;
        }
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
        self.terminal_version.clear();
        self.terminal_multiplexer.clear();
        self.terminal_override = false;
        self.originator = ORIGINATOR.into();
    }

    /// Brings a stored identity (possibly hand-edited by an older version)
    /// back to its platform's preset.
    fn normalize(&mut self) {
        let terminal_override = self.terminal_override;
        let terminal = self.terminal.clone();
        let terminal_version = self.terminal_version.clone();
        let terminal_multiplexer = self.terminal_multiplexer.clone();
        self.set_platform(self.platform());
        if terminal_override {
            self.terminal = terminal;
            self.terminal_version = terminal_version;
            self.terminal_multiplexer = terminal_multiplexer;
            self.terminal_override = true;
        }
        if !is_version(self.cli_version.trim()) {
            self.cli_version = DEFAULT_VERSION.into();
        }
    }

    /// Detect the terminal environment once per identity load. The selected
    /// virtual platform remains authoritative for OS/version/architecture;
    /// terminal detection is safe to use independently because it is the
    /// same environment metadata the official CLI includes in its UA.
    fn detect_runtime_terminal(&mut self) {
        let Some(detected) = detect_terminal_info() else {
            return;
        };
        self.terminal = detected.name;
        self.terminal_version = detected.version;
        self.terminal_multiplexer = detected.multiplexer;
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
            terminal_version: String::new(),
            terminal_multiplexer: String::new(),
            terminal_override: false,
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
    if value.is_empty() || value.len() > 64 * 1024 {
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
        parent_thread_id: metadata.and_then(|m| {
            safe_metadata_string(m.get("x-codex-parent-thread-id"))
                .or_else(|| safe_metadata_string(m.get("parent_thread_id")))
        }),
        parent_turn_id: metadata.and_then(|m| safe_metadata_string(m.get("parent_turn_id"))),
        root_turn_id: metadata.and_then(|m| safe_metadata_string(m.get("root_turn_id"))),
        forked_from_thread_id: metadata
            .and_then(|m| safe_metadata_string(m.get("forked_from_thread_id"))),
        context_window_id: metadata.and_then(|m| safe_metadata_string(m.get("context_window_id"))),
        window_number: metadata.and_then(|m| m.get("window_number").and_then(Value::as_u64)),
        agent_name: metadata.and_then(|m| safe_metadata_string(m.get("agent_name"))),
        thread_source: metadata.and_then(|m| safe_metadata_string(m.get("thread_source"))),
        turn_trigger: metadata.and_then(|m| safe_metadata_string(m.get("turn_trigger"))),
        request_kind: metadata.and_then(|m| safe_metadata_string(m.get("request_kind"))),
        sandbox: metadata.and_then(|m| safe_metadata_string(m.get("sandbox"))),
        sandbox_mode: metadata.and_then(|m| safe_metadata_string(m.get("sandbox_mode"))),
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
            if context.parent_turn_id.is_none() {
                context.parent_turn_id = safe_metadata_string(snapshot.get("parent_turn_id"));
            }
            if context.root_turn_id.is_none() {
                context.root_turn_id = safe_metadata_string(snapshot.get("root_turn_id"));
            }
            if context.forked_from_thread_id.is_none() {
                context.forked_from_thread_id =
                    safe_metadata_string(snapshot.get("forked_from_thread_id"));
            }
            if context.context_window_id.is_none() {
                context.context_window_id = safe_metadata_string(snapshot.get("context_window_id"));
            }
            if context.window_number.is_none() {
                context.window_number = snapshot.get("window_number").and_then(Value::as_u64);
            }
            if context.agent_name.is_none() {
                context.agent_name = safe_metadata_string(snapshot.get("agent_name"));
            }
            if context.thread_source.is_none() {
                context.thread_source = safe_metadata_string(snapshot.get("thread_source"));
            }
            if context.turn_trigger.is_none() {
                context.turn_trigger = safe_metadata_string(snapshot.get("turn_trigger"));
            }
            if context.request_kind.is_none() {
                context.request_kind = safe_metadata_string(snapshot.get("request_kind"));
            }
            if context.sandbox.is_none() {
                context.sandbox = safe_metadata_string(snapshot.get("sandbox"));
            }
            if context.sandbox_mode.is_none() {
                context.sandbox_mode = safe_metadata_string(snapshot.get("sandbox_mode"));
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
    sanitize_turn_metadata_snapshot(&mut snapshot, identity);
    snapshot.insert("installation_id".into(), json!(identity.installation_id));
    snapshot.insert("session_id".into(), json!(identity.session_id));
    snapshot.insert("window_id".into(), json!(identity.window_id));
    snapshot.insert("thread_id".into(), json!(identity.thread_id));
    if let Some(turn_id) = context.turn_id.as_deref() {
        snapshot.insert(
            "turn_id".into(),
            json!(identity.map_protocol_id("turn", turn_id)),
        );
    }
    if let Some(parent_thread_id) = context.parent_thread_id.as_deref() {
        snapshot.insert(
            "parent_thread_id".into(),
            json!(identity.map_protocol_id("thread", parent_thread_id)),
        );
    }
    if let Some(subagent) = context.subagent.as_deref() {
        snapshot.insert("subagent_kind".into(), json!(subagent));
    }
    for (key, value) in [
        ("agent_name", context.agent_name.as_deref()),
        ("thread_source", context.thread_source.as_deref()),
        ("turn_trigger", context.turn_trigger.as_deref()),
        ("request_kind", context.request_kind.as_deref()),
        ("sandbox", context.sandbox.as_deref()),
        ("sandbox_mode", context.sandbox_mode.as_deref()),
    ] {
        if let Some(value) = value {
            snapshot.insert(key.into(), json!(value));
        }
    }
    if let Some(window_number) = context.window_number {
        snapshot.insert("window_number".into(), json!(window_number));
    }
    for (key, kind) in [
        ("parent_turn_id", "turn"),
        ("root_turn_id", "turn"),
        ("forked_from_thread_id", "thread"),
        ("context_window_id", "context-window"),
    ] {
        let source = snapshot.get(key).and_then(Value::as_str).map(str::to_owned);
        if let Some(source) = source {
            snapshot.insert(key.into(), json!(identity.map_protocol_id(kind, &source)));
        }
    }
    metadata.insert(
        "x-codex-turn-metadata".into(),
        Value::String(ascii_json(&Value::Object(snapshot))),
    );
}

/// Keep the upstream metadata shape while preventing old or custom clients
/// from mixing host execution details into a newly virtualized lineage.  The
/// official fields remain available; oversized tool/MCP/workspace payloads
/// are dropped at the same boundary where Codex bounds them for headers.
fn sanitize_turn_metadata_snapshot(
    snapshot: &mut serde_json::Map<String, Value>,
    identity: &VmIdentity,
) {
    const HOST_KEYS: &[&str] = &[
        "cwd",
        "shell",
        "shell_version",
        "hostname",
        "host_name",
        "host_id",
        "machine_id",
        "workspace_root",
        "filesystem",
        "filesystem_access",
        "network",
        "network_access",
        "subagents",
        "subagent_environment",
        "environment_id",
        "environment_ids",
    ];
    snapshot.retain(|key, _| !HOST_KEYS.iter().any(|host| key == host));
    sanitize_workspace_metadata(snapshot, identity);
    for key in [
        "workspaces",
        "tool_namespaces_info",
        "mcp_attribution",
        "extra",
    ] {
        let oversized = snapshot
            .get(key)
            .and_then(|value| serde_json::to_vec(value).ok())
            .is_some_and(|bytes| bytes.len() > 16 * 1024);
        if oversized {
            snapshot.remove(key);
        }
    }
}

fn sanitize_workspace_metadata(
    snapshot: &mut serde_json::Map<String, Value>,
    identity: &VmIdentity,
) {
    let Some(Value::Object(workspaces)) = snapshot.remove("workspaces") else {
        return;
    };
    let mut sanitized = serde_json::Map::new();
    for (name, mut value) in workspaces.into_iter().take(32) {
        let key = if name.len() <= 64
            && name.is_ascii()
            && !name
                .chars()
                .any(|ch| ch == '/' || ch == '\\' || ch.is_control())
        {
            name
        } else {
            identity.map_protocol_id("workspace", &name)
        };
        if let Value::Object(fields) = &mut value {
            fields.retain(|field, _| {
                matches!(
                    field.as_str(),
                    "associated_remote_urls" | "latest_git_commit_hash" | "has_changes"
                )
            });
            if let Some(Value::String(hash)) = fields.get_mut("latest_git_commit_hash") {
                if hash.len() > 128 || !hash.is_ascii() {
                    fields.remove("latest_git_commit_hash");
                }
            }
            if let Some(Value::Object(urls)) = fields.get_mut("associated_remote_urls") {
                urls.retain(|remote, value| {
                    if remote.len() > 64 || !remote.is_ascii() {
                        return false;
                    }
                    let Some(url) = value.as_str() else {
                        return false;
                    };
                    *value = Value::String(sanitize_remote_url(url));
                    true
                });
            }
        } else {
            value = Value::Object(serde_json::Map::new());
        }
        sanitized.insert(key, value);
    }
    snapshot.insert("workspaces".into(), Value::Object(sanitized));
}

fn sanitize_remote_url(value: &str) -> String {
    let Ok(mut url) = url::Url::parse(value) else {
        return value
            .chars()
            .filter(|ch| ch.is_ascii_graphic())
            .take(512)
            .collect();
    };
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string().chars().take(512).collect()
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
            // The official environment_context has no virtual_device or
            // locale elements. Remove Kit-private tags and replace the host
            // execution fields with the selected virtual platform instead.
            rewrite_environment_ids(&mut next, identity);
            for tag in [
                "virtual_device",
                "locale",
                "shell_version",
                "network_access",
                "network",
                "filesystem",
                "filesystem_access",
                "subagents",
            ] {
                remove_context_tag_all(&mut next, tag);
            }
            set_context_tag_all(&mut next, "cwd", virtual_cwd(identity));
            set_context_tag_all(&mut next, "shell", virtual_shell(identity));
            let timezone = environment
                .timezone
                .parse::<chrono_tz::Tz>()
                .ok()
                .map(|_| environment.timezone.as_str())
                .unwrap_or("UTC");
            let tz = timezone.parse::<chrono_tz::Tz>().unwrap_or(chrono_tz::UTC);
            set_context_tag_all(&mut next, "timezone", timezone);
            let date = chrono::Utc::now()
                .with_timezone(&tz)
                .format("%Y-%m-%d")
                .to_string();
            set_context_tag_all(&mut next, "current_date", &date);
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

/// Environment IDs are protocol lineage, not a reason to expose the source
/// harness's identifiers.  Keep their ordering and UUID shape while mapping
/// every nested `<environment id="…">` into the virtual device namespace.
fn rewrite_environment_ids(text: &mut String, identity: &VmIdentity) {
    let mut cursor = 0;
    let mut ordinal = 0u64;
    loop {
        let Some(offset) = text[cursor..].find("<environment") else {
            break;
        };
        let start = cursor + offset;
        let name_end = start + "<environment".len();
        let Some(next) = text[name_end..].chars().next() else {
            break;
        };
        // Do not treat the root `<environment_context>` element as an
        // execution environment.
        if next == '_' || (!next.is_whitespace() && next != '>') {
            cursor = name_end;
            continue;
        }
        let Some(end_offset) = text[name_end..].find('>') else {
            break;
        };
        let end = name_end + end_offset;
        let tag = text[start..=end].to_owned();
        let marker = tag
            .find(" id=\"")
            .map(|offset| (offset + " id=\"".len(), '"'))
            .or_else(|| {
                tag.find(" id='")
                    .map(|offset| (offset + " id='".len(), '\''))
            });
        if let Some((value_offset, quote)) = marker {
            if let Some(close_offset) = tag[value_offset..].find(quote) {
                let value_start = start + value_offset;
                let value_end = value_start + close_offset;
                let source = text[value_start..value_end].to_owned();
                let mapped = identity
                    .map_protocol_id("environment", &format!("{ordinal}:{}", source.trim()));
                text.replace_range(value_start..value_end, &mapped);
                ordinal = ordinal.saturating_add(1);
            }
        }
        cursor = end.saturating_add(1);
    }
}

fn set_context_tag_all(text: &mut String, name: &str, value: &str) {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut search_from = 0;
    let mut found = false;
    while let Some(open_start) = text[search_from..].find(&open) {
        let open_start = search_from + open_start;
        let value_start = open_start + open.len();
        let Some(close_offset) = text[value_start..].find(&close) else {
            break;
        };
        let close_start = value_start + close_offset;
        text.replace_range(value_start..close_start, value);
        search_from = value_start + value.len() + close.len();
        found = true;
    }
    if !found {
        set_context_tag(text, name, value);
    }
}

fn remove_context_tag_all(text: &mut String, name: &str) {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut search_from = 0;
    while let Some(open_offset) = text[search_from..].find(&open) {
        let open_start = search_from + open_offset;
        let Some(close_offset) = text[open_start + open.len()..].find(&close) else {
            break;
        };
        let end = open_start + open.len() + close_offset + close.len();
        text.replace_range(open_start..end, "");
        search_from = open_start;
    }
}

fn virtual_cwd(identity: &VmIdentity) -> &'static str {
    match identity.platform() {
        DevicePlatform::Mac => "/Users/codex",
        DevicePlatform::Windows => "C:\\Users\\codex",
        DevicePlatform::Linux => "/home/codex",
    }
}

fn virtual_shell(identity: &VmIdentity) -> &'static str {
    match identity.platform() {
        DevicePlatform::Mac => "/bin/zsh",
        DevicePlatform::Windows => "powershell",
        DevicePlatform::Linux => "/bin/bash",
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
        if context.parent_turn_id.is_none() {
            context.parent_turn_id = request_context.parent_turn_id.clone();
        }
        if context.root_turn_id.is_none() {
            context.root_turn_id = request_context.root_turn_id.clone();
        }
        if context.forked_from_thread_id.is_none() {
            context.forked_from_thread_id = request_context.forked_from_thread_id.clone();
        }
        if context.context_window_id.is_none() {
            context.context_window_id = request_context.context_window_id.clone();
        }
        if context.window_number.is_none() {
            context.window_number = request_context.window_number;
        }
        if context.agent_name.is_none() {
            context.agent_name = request_context.agent_name.clone();
        }
        if context.thread_source.is_none() {
            context.thread_source = request_context.thread_source.clone();
        }
        if context.turn_trigger.is_none() {
            context.turn_trigger = request_context.turn_trigger.clone();
        }
        if context.request_kind.is_none() {
            context.request_kind = request_context.request_kind.clone();
        }
        if context.sandbox.is_none() {
            context.sandbox = request_context.sandbox.clone();
        }
        if context.sandbox_mode.is_none() {
            context.sandbox_mode = request_context.sandbox_mode.clone();
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
    metadata.insert("thread_id".into(), json!(identity.thread_id));
    for (key, kind, source) in [
        ("turn_id", "turn", context.turn_id.as_deref()),
        (
            "parent_thread_id",
            "thread",
            context.parent_thread_id.as_deref(),
        ),
        ("parent_turn_id", "turn", context.parent_turn_id.as_deref()),
        ("root_turn_id", "turn", context.root_turn_id.as_deref()),
        (
            "forked_from_thread_id",
            "thread",
            context.forked_from_thread_id.as_deref(),
        ),
        (
            "context_window_id",
            "context-window",
            context.context_window_id.as_deref(),
        ),
    ] {
        if let Some(source) = source {
            metadata.insert(key.into(), json!(identity.map_protocol_id(kind, source)));
        }
    }
    if let Some(parent_thread_id) = context.parent_thread_id.as_deref() {
        metadata.insert(
            "x-codex-parent-thread-id".into(),
            json!(identity.map_protocol_id("thread", parent_thread_id)),
        );
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

/// Mirrors Codex's process-wide `USER_AGENT_SUFFIX` hook while keeping the
/// value safe for an HTTP header.  `CODEX_USER_AGENT_SUFFIX` is accepted as a
/// namespaced alias for shells that reserve the generic variable name.
fn user_agent_suffix() -> Option<String> {
    ["USER_AGENT_SUFFIX", "CODEX_USER_AGENT_SUFFIX"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find_map(|value| {
            let value = sanitize_user_agent_suffix(&value);
            (!value.is_empty()).then_some(value)
        })
}

fn sanitize_user_agent_suffix(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_graphic() || ch == ' ' {
                ch
            } else {
                '_'
            }
        })
        .take(128)
        .collect()
}

fn valid_uuid(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DetectedTerminal {
    name: String,
    version: String,
    multiplexer: String,
}

/// Detect terminal metadata using the same environment precedence as the
/// upstream Codex terminal-detection crate. The raw values are sanitized
/// before they can reach a User-Agent header or persisted profile.
fn detect_terminal_info() -> Option<DetectedTerminal> {
    detect_terminal_info_with(|name| std::env::var(name).ok())
}

fn detect_terminal_info_with<F>(get: F) -> Option<DetectedTerminal>
where
    F: Fn(&str) -> Option<String> + Copy,
{
    let multiplexer = detect_multiplexer_with(get);

    if let Some(term_program) = env_non_empty_with(get, "TERM_PROGRAM") {
        if !term_program.eq_ignore_ascii_case("tmux") {
            return Some(DetectedTerminal {
                name: sanitize_terminal_token(&term_program),
                version: env_non_empty_with(get, "TERM_PROGRAM_VERSION")
                    .map(|value| sanitize_terminal_token(&value))
                    .unwrap_or_default(),
                multiplexer,
            });
        }
    }

    let detected = if get("GHOSTTY_RESOURCES_DIR").is_some() {
        Some(("Ghostty".to_string(), None))
    } else if get("WEZTERM_VERSION").is_some() {
        Some((
            "WezTerm".to_string(),
            env_non_empty_with(get, "WEZTERM_VERSION"),
        ))
    } else if get("ITERM_SESSION_ID").is_some()
        || get("ITERM_PROFILE").is_some()
        || get("ITERM_PROFILE_NAME").is_some()
    {
        Some(("iTerm.app".to_string(), None))
    } else if get("TERM_SESSION_ID").is_some() {
        Some(("Apple_Terminal".to_string(), None))
    } else if get("KITTY_WINDOW_ID").is_some()
        || get("TERM")
            .map(|value| value.contains("kitty"))
            .unwrap_or(false)
    {
        Some(("kitty".to_string(), None))
    } else if get("ALACRITTY_SOCKET").is_some()
        || get("TERM")
            .map(|value| value == "alacritty")
            .unwrap_or(false)
    {
        Some(("Alacritty".to_string(), None))
    } else if get("KONSOLE_VERSION").is_some() {
        Some((
            "Konsole".to_string(),
            env_non_empty_with(get, "KONSOLE_VERSION"),
        ))
    } else if get("GNOME_TERMINAL_SCREEN").is_some() {
        Some(("gnome-terminal".to_string(), None))
    } else if get("VTE_VERSION").is_some() {
        Some(("VTE".to_string(), env_non_empty_with(get, "VTE_VERSION")))
    } else if get("WT_SESSION").is_some() {
        Some(("WindowsTerminal".to_string(), None))
    } else {
        env_non_empty_with(get, "TERM").map(|term| (term, None))
    };

    detected.map(|(name, version)| DetectedTerminal {
        name: sanitize_terminal_token(&name),
        version: version
            .as_deref()
            .map(sanitize_terminal_token)
            .unwrap_or_default(),
        multiplexer,
    })
}

fn detect_multiplexer_with<F>(get: F) -> String
where
    F: Fn(&str) -> Option<String> + Copy,
{
    if get("TMUX").is_some() || get("TMUX_PANE").is_some() {
        let version = if get("TERM_PROGRAM")
            .map(|value| value.eq_ignore_ascii_case("tmux"))
            .unwrap_or(false)
        {
            env_non_empty_with(get, "TERM_PROGRAM_VERSION")
        } else {
            None
        };
        return format_multiplexer("tmux", version.as_deref());
    }
    if get("ZELLIJ").is_some()
        || get("ZELLIJ_SESSION_NAME").is_some()
        || get("ZELLIJ_VERSION").is_some()
    {
        return format_multiplexer(
            "zellij",
            env_non_empty_with(get, "ZELLIJ_VERSION").as_deref(),
        );
    }
    String::new()
}

fn format_multiplexer(name: &str, version: Option<&str>) -> String {
    let name = sanitize_terminal_token(name);
    match version.map(sanitize_terminal_token) {
        Some(version) if !version.is_empty() => format!("{name}/{version}"),
        _ => name,
    }
}

fn env_non_empty_with<F>(get: F, name: &str) -> Option<String>
where
    F: Fn(&str) -> Option<String>,
{
    get(name).filter(|value| !value.trim().is_empty())
}

fn sanitize_terminal_token(value: &str) -> String {
    value
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/') {
                ch
            } else {
                '_'
            }
        })
        .collect()
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
        assert!(text.contains("<cwd>/Users/codex</cwd>"));
        assert!(text.contains("<shell>/bin/zsh</shell>"));
        assert!(!text.contains("<locale>"));
        assert!(!text.contains("<virtual_device>"));
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
        assert_eq!(snapshot["thread_id"], identity.thread_id);
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
    fn newer_canonical_metadata_is_preserved_but_protocol_ids_are_virtualized() {
        let identity = VmIdentity::ephemeral();
        let mut body = json!({
            "model": "m",
            "input": [],
            "client_metadata": {
                "x-codex-turn-metadata": r#"{
                    "installation_id":"old-install",
                    "session_id":"old-session",
                    "window_id":"old-window",
                    "thread_id":"old-thread",
                    "turn_id":"old-turn",
                    "window_number":3,
                    "context_window_id":"old-context",
                    "parent_turn_id":"old-parent",
                    "thread_source":"user",
                    "request_kind":"turn",
                    "sandbox_mode":"workspace-write"
                }"#
            }
        });
        assert!(rewrite_client_metadata_value(&mut body, &identity));
        let snapshot: Value = serde_json::from_str(
            body["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot["installation_id"], identity.installation_id);
        assert_eq!(snapshot["session_id"], identity.session_id);
        assert_eq!(snapshot["thread_id"], identity.thread_id);
        assert_eq!(snapshot["window_number"], 3);
        assert_eq!(snapshot["thread_source"], "user");
        assert_eq!(snapshot["request_kind"], "turn");
        assert_eq!(snapshot["sandbox_mode"], "workspace-write");
        for key in ["context_window_id", "parent_turn_id", "turn_id"] {
            assert_ne!(snapshot[key], Value::String(format!("old-{key}")));
            assert_eq!(
                Uuid::parse_str(snapshot[key].as_str().unwrap())
                    .unwrap()
                    .get_version_num(),
                7
            );
        }
    }

    #[test]
    fn canonical_metadata_drops_host_fields_but_keeps_bounded_protocol_extensions() {
        let identity = VmIdentity::ephemeral();
        let mut body = json!({
            "model": "m",
            "input": [],
            "client_metadata": {
                "x-codex-turn-metadata": serde_json::json!({
                    "request_kind": "turn",
                    "hostname": "real-host",
                    "workspace_root": "/Users/real/project",
                    "analytics_enabled": true,
                    "future_protocol_field": "kept"
                }).to_string()
            }
        });
        assert!(rewrite_client_metadata_value(&mut body, &identity));
        let snapshot: Value = serde_json::from_str(
            body["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(snapshot.get("hostname").is_none());
        assert!(snapshot.get("workspace_root").is_none());
        assert_eq!(snapshot["analytics_enabled"], true);
        assert_eq!(snapshot["future_protocol_field"], "kept");
    }

    #[test]
    fn workspace_metadata_maps_paths_and_strips_git_credentials() {
        let identity = VmIdentity::ephemeral();
        let mut body = json!({
            "model": "m",
            "input": [],
            "client_metadata": {
                "x-codex-turn-metadata": serde_json::json!({
                    "request_kind": "turn",
                    "workspaces": {
                        "/Users/real/project": {
                            "associated_remote_urls": {
                                "origin": "https://user:secret@example.com/repo.git"
                            },
                            "latest_git_commit_hash": "abc"
                        }
                    }
                }).to_string()
            }
        });
        assert!(rewrite_client_metadata_value(&mut body, &identity));
        let snapshot: Value = serde_json::from_str(
            body["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let workspace = snapshot["workspaces"].as_object().unwrap();
        assert!(!workspace.contains_key("/Users/real/project"));
        let remote = workspace.values().next().unwrap()["associated_remote_urls"]["origin"]
            .as_str()
            .unwrap();
        assert_eq!(remote, "https://example.com/repo.git");
    }

    #[test]
    fn nested_environment_context_replaces_all_host_execution_fields() {
        let identity = VmIdentity::ephemeral();
        let text = "<environment_context><environments><environment id=\"one\"><cwd>/host/a</cwd><shell>/bin/fish</shell><shell_version>3</shell_version></environment><environment id=\"two\"><cwd>/host/b</cwd><shell>/bin/fish</shell></environment></environments><current_date>2000-01-01</current_date><timezone>Asia/Tokyo</timezone><network>allowed</network><filesystem>host</filesystem><subagents>host</subagents></environment_context>";
        let mut body =
            json!({"input":[{"role":"user","content":[{"type":"input_text","text":text}]}]});
        assert!(rewrite_client_metadata_value(&mut body, &identity));
        let rendered = body["input"][0]["content"][0]["text"].as_str().unwrap();
        assert_eq!(rendered.matches("<cwd>/Users/codex</cwd>").count(), 2);
        assert_eq!(rendered.matches("<shell>/bin/zsh</shell>").count(), 2);
        assert!(!rendered.contains("id=\"one\"") && !rendered.contains("id=\"two\""));
        assert!(!rendered.contains("/host/"));
        assert!(!rendered.contains("shell_version"));
        assert!(!rendered.contains("<network>"));
        assert!(!rendered.contains("<filesystem>"));
        assert!(!rendered.contains("<subagents>"));
        assert!(!rendered.contains("2000-01-01"));
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
        assert_ne!(first_mapping.thread_id, "thread-a");
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
        assert_eq!(first.session_id, second.session_id);
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
        assert!(identity.user_agent().starts_with(&format!(
            "{}/0.155.0 (Mac OS 15.5.0; arm64) ",
            identity.originator_value()
        )));
        assert!(identity.user_agent().contains("xterm-256color"));
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
        assert_eq!(
            value["client_metadata"]["thread_id"],
            json!(identity.thread_id)
        );
        assert_eq!(
            value["client_metadata"]["turn_id"],
            json!(identity.map_protocol_id("turn", "turn-1"))
        );
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
            terminal: None,
            terminal_version: None,
            terminal_multiplexer: None,
        });
        assert_eq!(identity.platform(), DevicePlatform::Windows);
        assert!(identity.user_agent().starts_with(&format!(
            "{}/0.155.0 (Windows 10.0.26100; x86_64) ",
            identity.originator_value()
        )));
        assert!(identity.user_agent().contains("WindowsTerminal"));
        identity.apply_profile(VmProfile {
            platform: DevicePlatform::Linux,
            environment: None,
            enabled: None,
            terminal: None,
            terminal_version: None,
            terminal_multiplexer: None,
        });
        assert!(identity.user_agent().starts_with(&format!(
            "{}/0.155.0 (Ubuntu 24.4.0; x86_64) ",
            identity.originator_value()
        )));
        assert!(identity.user_agent().contains("xterm-256color"));
        identity.apply_profile(VmProfile {
            platform: DevicePlatform::Linux,
            environment: None,
            enabled: Some(false),
            terminal: None,
            terminal_version: None,
            terminal_multiplexer: None,
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
        assert!(identity.user_agent().starts_with(&format!(
            "{}/0.155.0 (Windows 10.0.26100; x86_64) ",
            identity.originator_value()
        )));
        assert!(identity.user_agent().contains("WindowsTerminal"));
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

    #[test]
    fn detects_terminal_program_version_and_multiplexer() {
        let values = std::collections::HashMap::from([
            ("TERM_PROGRAM", "iTerm2"),
            ("TERM_PROGRAM_VERSION", "3.5.1"),
            ("TMUX", "/tmp/tmux-1000/default,1,0"),
            ("TERM", "xterm-256color"),
        ]);
        let detected =
            detect_terminal_info_with(|name| values.get(name).map(|value| value.to_string()))
                .unwrap();
        assert_eq!(detected.name, "iTerm2");
        assert_eq!(detected.version, "3.5.1");
        assert_eq!(detected.multiplexer, "tmux");
    }

    #[test]
    fn tmux_term_program_falls_back_to_underlying_terminal() {
        let values = std::collections::HashMap::from([
            ("TERM_PROGRAM", "tmux"),
            ("TERM_PROGRAM_VERSION", "3.4"),
            ("TMUX", "1"),
            ("TERM", "screen-256color"),
        ]);
        let detected =
            detect_terminal_info_with(|name| values.get(name).map(|value| value.to_string()))
                .unwrap();
        assert_eq!(detected.name, "screen-256color");
        assert_eq!(detected.version, "");
        assert_eq!(detected.multiplexer, "tmux/3.4");
    }

    #[test]
    fn terminal_version_is_included_in_user_agent_without_changing_legacy_shape() {
        let mut identity = VmIdentity::ephemeral();
        identity.terminal = "Ghostty".into();
        identity.terminal_version = "1.2.3".into();
        identity.terminal_multiplexer = "zellij/0.40".into();
        assert!(identity.user_agent().starts_with(&format!(
            "{}/0.155.0 (Mac OS 15.5.0; arm64) Ghostty/1.2.3",
            identity.originator_value()
        )));
        let value = serde_json::to_value(&identity).unwrap();
        assert_eq!(value["terminalVersion"], "1.2.3");
        assert_eq!(value["terminalMultiplexer"], "zellij/0.40");
    }

    #[test]
    fn user_agent_suffix_is_sanitized_for_header_use() {
        assert_eq!(
            sanitize_user_agent_suffix("  mcp\nclient\u{1f600}  "),
            "mcp_client_  ".trim()
        );
    }

    #[test]
    fn old_profiles_default_new_terminal_fields() {
        let raw = r#"{
            "installationId":"00000000-0000-4000-8000-000000000000",
            "cliVersion":"0.160.0",
            "originator":"codex_cli_rs",
            "osType":"Linux",
            "osVersion":"6.8.0",
            "arch":"x86_64",
            "terminal":"xterm-256color"
        }"#;
        let identity: VmIdentity = serde_json::from_str(raw).unwrap();
        assert_eq!(identity.terminal_version, "");
        assert_eq!(identity.terminal_multiplexer, "");
        assert!(!identity.terminal_override);
    }

    #[test]
    fn old_vm_profiles_default_optional_terminal_overrides() {
        let profile: VmProfile =
            serde_json::from_str(r#"{"platform":"windows","environment":null,"enabled":true}"#)
                .unwrap();
        assert_eq!(profile.platform, DevicePlatform::Windows);
        assert_eq!(profile.enabled, Some(true));
        assert!(profile.terminal.is_none());
        assert!(profile.terminal_version.is_none());
        assert!(profile.terminal_multiplexer.is_none());
    }
}
