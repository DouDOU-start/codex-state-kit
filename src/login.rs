use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use http::{HeaderMap, HeaderName, HeaderValue};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Kit 独立登录文件。接入期间会覆盖官方 `auth.json`，退出时从备份还原。
pub const KIT_AUTH_FILE: &str = "auth.codex-state-kit.json";
pub const OFFICIAL_AUTH_BACKUP_FILE: &str = "auth.json.codex-state-kit.bak";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LoginMethod {
    Device,
    #[default]
    Browser,
}

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const USER_AGENT: &str = "codex-state-kit";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn oauth_token_endpoint() -> &'static str {
    OAUTH_TOKEN_URL
}
const VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEFAULT_EXPIRES_IN: u64 = 900;
const DEFAULT_INTERVAL: u64 = 5;
static AUTH_SYNC_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Serialises writes of the Kit login file (token adoption, account switches).
pub(crate) fn auth_sync_lock() -> std::sync::MutexGuard<'static, ()> {
    AUTH_SYNC_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

#[derive(Clone, Debug)]
pub struct LoginEndpoints {
    pub usercode_url: String,
    pub poll_url: String,
    pub oauth_token_url: String,
}

impl Default for LoginEndpoints {
    fn default() -> Self {
        Self {
            usercode_url: "https://auth.openai.com/api/accounts/deviceauth/usercode".into(),
            poll_url: "https://auth.openai.com/api/accounts/deviceauth/token".into(),
            oauth_token_url: OAUTH_TOKEN_URL.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStatus {
    pub logged_in: bool,
    pub auth_mode: Option<String>,
    pub email: Option<String>,
    pub account_id: Option<String>,
    pub refreshable: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginStart {
    pub method: LoginMethod,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Clone, Debug)]
pub struct PendingLogin {
    cancelled: Arc<Mutex<bool>>,
    pub device_auth_id: String,
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub expires_at: Instant,
    pub home: PathBuf,
}

impl PendingLogin {
    pub fn cancel(&self) {
        *self.cancelled.lock().expect("login cancellation") = true;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PollStatus {
    Pending,
    Denied,
    Expired,
    Failed,
    Ok,
}

impl PollStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Denied => "denied",
            Self::Expired => "expired",
            Self::Failed => "error",
            Self::Ok => "ok",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PollResult {
    pub status: PollStatus,
    pub message: Option<String>,
    pub login: Option<LoginStatus>,
}

#[derive(Clone, Debug, Deserialize)]
struct DeviceCodeResponse {
    device_auth_id: String,
    user_code: String,
    #[serde(default)]
    interval: Option<Value>,
    #[serde(default)]
    expires_in: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
struct DevicePollSuccess {
    authorization_code: String,
    code_verifier: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct OAuthTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResolvedAuthMode {
    ApiKey,
    Chatgpt,
    ChatgptAuthTokens,
    Other,
}

/// Auth requests go out on the account's outbound line (`outbound_proxy`,
/// the same exit as its business traffic). With no line configured they
/// follow the system proxy, like a browser.
fn auth_client_builder(outbound_proxy: &str) -> Result<reqwest::ClientBuilder> {
    let proxy = crate::fetch::dial_proxy_for_client(outbound_proxy.trim());
    let builder = reqwest::Client::builder().timeout(Duration::from_secs(30));
    Ok(if proxy.is_empty() {
        builder.proxy(crate::system_proxy::reqwest_proxy())
    } else {
        builder.proxy(reqwest::Proxy::all(&proxy).context("出站代理地址无效")?)
    })
}

pub fn http_client_via(outbound_proxy: &str) -> Result<reqwest::Client> {
    auth_client_builder(outbound_proxy)?
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .context("build login http client")
}

/// RT 换票请求携带长期凭据，禁止跟随重定向，避免请求体被转发到其他来源。
pub fn token_import_http_client_via(outbound_proxy: &str) -> Result<reqwest::Client> {
    auth_client_builder(outbound_proxy)?
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("build token import http client")
}

pub fn http_client() -> Result<reqwest::Client> {
    http_client_via("")
}

pub fn token_import_http_client() -> Result<reqwest::Client> {
    token_import_http_client_via("")
}

pub fn kit_auth_path(home: &Path) -> PathBuf {
    home.join(KIT_AUTH_FILE)
}

pub fn official_auth_backup_path(home: &Path) -> PathBuf {
    home.join(OFFICIAL_AUTH_BACKUP_FILE)
}

fn official_auth_path(home: &Path) -> PathBuf {
    home.join("auth.json")
}

/// 首次覆盖前把官方 `auth.json` 备份到同目录，再把 Kit 登录同步过去。
pub fn overlay_kit_onto_official(home: &Path) -> Result<()> {
    capture_official_auth_once(home)?;
    let kit = kit_auth_path(home);
    if kit.exists() {
        std::fs::copy(&kit, official_auth_path(home))
            .with_context(|| format!("sync {}", official_auth_path(home).display()))?;
    }
    Ok(())
}

/// 退出 Kit 时还原打开前的官方账号文件。
pub fn restore_official_auth(home: &Path) -> Result<()> {
    let bak = official_auth_backup_path(home);
    if !bak.exists() {
        return Ok(());
    }
    let official = official_auth_path(home);
    let data = std::fs::read(&bak).with_context(|| format!("read {}", bak.display()))?;
    if data.is_empty() {
        let _ = std::fs::remove_file(&official);
    } else {
        atomic_write(&official, &data)?;
    }
    let _ = std::fs::remove_file(&bak);
    Ok(())
}

fn capture_official_auth_once(home: &Path) -> Result<()> {
    let bak = official_auth_backup_path(home);
    if bak.exists() {
        return Ok(());
    }
    if let Some(parent) = bak.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let official = official_auth_path(home);
    match std::fs::read(&official) {
        Ok(bytes) => {
            std::fs::write(&bak, bytes).with_context(|| format!("write {}", bak.display()))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            std::fs::write(&bak, b"").with_context(|| format!("write {}", bak.display()))?;
        }
        Err(err) => {
            return Err(err).with_context(|| format!("read {}", official.display()));
        }
    }
    Ok(())
}

fn empty_login_status() -> LoginStatus {
    LoginStatus {
        logged_in: false,
        auth_mode: None,
        email: None,
        account_id: None,
        refreshable: false,
    }
}

fn status_from_file(path: &Path) -> LoginStatus {
    match read_auth_file(path) {
        Ok(Some(auth)) => status_from_auth(&auth),
        _ => empty_login_status(),
    }
}

/// 优先显示 Kit 独立登录；没有时才回落到官方 auth.json。
pub fn login_status(home: &Path) -> LoginStatus {
    let kit = status_from_file(&kit_auth_path(home));
    if kit.logged_in {
        return kit;
    }
    status_from_file(&official_auth_path(home))
}

pub fn has_kit_session(home: &Path) -> bool {
    status_from_file(&kit_auth_path(home)).logged_in
}

pub fn has_chatgpt_login(home: &Path) -> bool {
    login_status(home).logged_in
}

#[derive(Clone, Debug)]
pub(crate) struct ChatGptCredentials {
    pub access_token: String,
    pub account_id: String,
    pub email: Option<String>,
    pub auth_mode: String,
    pub refreshable: bool,
}

pub(crate) fn request_credentials(home: &Path) -> Result<(ChatGptCredentials, bool)> {
    let kit_path = kit_auth_path(home);
    if let Some(kit_auth) = read_auth_file(&kit_path)? {
        if status_from_auth(&kit_auth).logged_in {
            // Codex 会把轮换后的 OAuth 凭据写回官方 auth.json。Kit 独立登录
            // 仍负责覆盖请求头，因此同账号且更新的官方凭据必须回写到 Kit，
            // 否则会继续使用已经过期的 access token。
            if let Ok(Some(official_auth)) = read_auth_file(&official_auth_path(home)) {
                if should_adopt_official_refresh(&kit_auth, &official_auth) {
                    let _guard = AUTH_SYNC_LOCK
                        .get_or_init(|| Mutex::new(()))
                        .lock()
                        .expect("auth sync lock");
                    if let (Ok(Some(latest_kit)), Ok(Some(latest_official))) = (
                        read_auth_file(&kit_path),
                        read_auth_file(&official_auth_path(home)),
                    ) {
                        if should_adopt_official_refresh(&latest_kit, &latest_official) {
                            // 即使持久化失败，本次请求也优先使用已由 Codex 刷新的同账号凭据。
                            let _ = atomic_write(
                                &kit_path,
                                &serde_json::to_vec_pretty(&latest_official)?,
                            );
                            return Ok((credentials_from_auth(&latest_official)?, true));
                        }
                    }
                }
            }
            return Ok((credentials_from_auth(&kit_auth)?, true));
        }
    }
    if let Some(creds) = credentials_from_file(&official_auth_path(home))? {
        return Ok((creds, false));
    }
    bail!("尚未登录 ChatGPT。请先在本应用完成 ChatGPT 登录。");
}

pub(crate) fn chatgpt_credentials(home: &Path) -> Result<ChatGptCredentials> {
    request_credentials(home).map(|(creds, _)| creds)
}

fn should_adopt_official_refresh(kit_auth: &Value, official_auth: &Value) -> bool {
    if resolved_auth_mode(kit_auth) != ResolvedAuthMode::Chatgpt
        || resolved_auth_mode(official_auth) != ResolvedAuthMode::Chatgpt
    {
        return false;
    }
    let kit_status = status_from_auth(kit_auth);
    let official_status = status_from_auth(official_auth);
    if !kit_status.logged_in
        || !official_status.logged_in
        || kit_status.account_id != official_status.account_id
        || kit_auth.get("tokens") == official_auth.get("tokens")
    {
        return false;
    }
    let refreshed_at = |auth: &Value| {
        auth.get("last_refresh")
            .and_then(Value::as_str)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
    };
    match (refreshed_at(kit_auth), refreshed_at(official_auth)) {
        (Some(kit), Some(official)) => official > kit,
        _ => false,
    }
}

fn credentials_from_file(path: &Path) -> Result<Option<ChatGptCredentials>> {
    let Some(auth) = read_auth_file(path)? else {
        return Ok(None);
    };
    if !status_from_auth(&auth).logged_in {
        return Ok(None);
    }
    Ok(Some(credentials_from_auth(&auth)?))
}

fn credentials_from_auth(auth: &Value) -> Result<ChatGptCredentials> {
    let tokens = auth
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("登录文件缺少 tokens"))?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("登录文件缺少 access_token"))?
        .to_string();
    let stored_account = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let id_token = tokens.get("id_token").and_then(Value::as_str).unwrap_or("");
    let (jwt_account, email) = extract_account_metadata(id_token, &access_token);
    let account_id = jwt_account
        .or(stored_account)
        .ok_or_else(|| anyhow::anyhow!("无法从登录文件提取 chatgpt_account_id"))?;
    let resolved_mode = resolved_auth_mode(auth);
    let refreshable = resolved_mode == ResolvedAuthMode::Chatgpt
        && tokens
            .get("refresh_token")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty());
    let auth_mode = match resolved_mode {
        ResolvedAuthMode::Chatgpt => "chatgpt",
        ResolvedAuthMode::ChatgptAuthTokens => "chatgptAuthTokens",
        _ => "chatgpt",
    }
    .to_string();
    Ok(ChatGptCredentials {
        access_token,
        account_id,
        email,
        auth_mode,
        refreshable,
    })
}

pub(crate) fn apply_chatgpt_credentials_headers(
    headers: &mut HeaderMap,
    creds: &ChatGptCredentials,
) -> bool {
    let (Ok(auth), Ok(account)) = (
        HeaderValue::from_str(&format!("Bearer {}", creds.access_token)),
        HeaderValue::from_str(&creds.account_id),
    ) else {
        return false;
    };
    headers.insert(http::header::AUTHORIZATION, auth);
    headers.insert(HeaderName::from_static("chatgpt-account-id"), account);
    crate::chatgpt_cookies::retain_allowed_request_cookies(headers);
    true
}

/// Kit 已独立登录时，用 Kit 账号替换转发请求上的鉴权头，官方客户端无需重启。
pub fn apply_kit_auth_headers(headers: &mut HeaderMap, home: &Path) -> Option<LoginStatus> {
    let Ok(Some(creds)) = credentials_from_file(&kit_auth_path(home)) else {
        return None;
    };
    if !apply_chatgpt_credentials_headers(headers, &creds) {
        return None;
    }
    Some(LoginStatus {
        logged_in: true,
        auth_mode: Some(creds.auth_mode),
        account_id: Some(creds.account_id),
        email: creds.email,
        refreshable: creds.refreshable,
    })
}

pub(crate) fn credentials_match_headers(headers: &HeaderMap, creds: &ChatGptCredentials) -> bool {
    request_account_id(headers).is_some_and(|account_id| account_id == creds.account_id)
}

/// Absence is not a conflict: Kit may supply the account header.
/// Only an explicit `chatgpt-account-id` for another account is a conflict.
/// A different Bearer on the same account is expected after AT refresh.
pub(crate) fn credentials_conflict_headers(
    headers: &HeaderMap,
    creds: &ChatGptCredentials,
) -> bool {
    request_account_id(headers).is_some_and(|account_id| account_id != creds.account_id)
}

fn request_account_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("chatgpt-account-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub async fn start_device_login(
    client: &reqwest::Client,
    endpoints: &LoginEndpoints,
    home: PathBuf,
) -> Result<(LoginStart, PendingLogin)> {
    let response = client
        .post(&endpoints.usercode_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&json!({ "client_id": CLIENT_ID }))
        .send()
        .await
        .context("start ChatGPT device login")?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        bail!("Device Code 请求失败: {status} - {text}");
    }
    let device: DeviceCodeResponse = response.json().await.context("parse device code")?;
    let interval = parse_interval(device.interval.as_ref());
    let expires_in = device.expires_in.unwrap_or(DEFAULT_EXPIRES_IN).max(1);
    let start = LoginStart {
        method: LoginMethod::Device,
        user_code: device.user_code.clone(),
        verification_uri: VERIFICATION_URI.to_string(),
        expires_in,
        interval,
    };
    let pending = PendingLogin {
        cancelled: Arc::new(Mutex::new(false)),
        device_auth_id: device.device_auth_id,
        user_code: device.user_code,
        verification_uri: VERIFICATION_URI.to_string(),
        expires_in,
        interval,
        expires_at: Instant::now() + Duration::from_secs(expires_in),
        home,
    };
    Ok((start, pending))
}

pub async fn poll_device_login(
    client: &reqwest::Client,
    endpoints: &LoginEndpoints,
    pending: &PendingLogin,
) -> Result<PollResult> {
    if *pending.cancelled.lock().expect("login cancellation") {
        bail!("登录已取消");
    }
    if Instant::now() >= pending.expires_at {
        return Ok(PollResult {
            status: PollStatus::Expired,
            message: Some("Device Code 已过期，请重新登录".into()),
            login: None,
        });
    }

    let response = client
        .post(&endpoints.poll_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", USER_AGENT)
        .json(&json!({
            "device_auth_id": pending.device_auth_id,
            "user_code": pending.user_code,
        }))
        .send()
        .await
        .context("poll ChatGPT device login")?;
    let status = response.status();
    if status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND {
        return Ok(PollResult {
            status: PollStatus::Pending,
            message: None,
            login: None,
        });
    }
    if status == StatusCode::GONE {
        return Ok(PollResult {
            status: PollStatus::Expired,
            message: Some("Device Code 已过期，请重新登录".into()),
            login: None,
        });
    }
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        if is_access_denied(&text) {
            return Ok(PollResult {
                status: PollStatus::Denied,
                message: Some("用户拒绝授权".into()),
                login: None,
            });
        }
        return Ok(PollResult {
            status: PollStatus::Failed,
            message: Some(format!("{status} - {text}")),
            login: None,
        });
    }

    let success: DevicePollSuccess = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(err) => {
            return Ok(PollResult {
                status: PollStatus::Failed,
                message: Some(format!("解析授权响应失败: {err}")),
                login: None,
            });
        }
    };

    match exchange_and_write(client, endpoints, &success, pending).await {
        Ok(login) => Ok(PollResult {
            status: PollStatus::Ok,
            message: Some("已保存并同步到 Codex 账号".into()),
            login: Some(login),
        }),
        Err(err) => Ok(PollResult {
            status: PollStatus::Failed,
            message: Some(err.to_string()),
            login: None,
        }),
    }
}

async fn exchange_and_write(
    client: &reqwest::Client,
    endpoints: &LoginEndpoints,
    success: &DevicePollSuccess,
    pending: &PendingLogin,
) -> Result<LoginStatus> {
    let tokens = exchange_tokens(
        client,
        &endpoints.oauth_token_url,
        &success.authorization_code,
        &success.code_verifier,
        REDIRECT_URI,
    )
    .await?;
    let cancelled = pending.cancelled.lock().expect("login cancellation");
    if *cancelled {
        bail!("登录已取消");
    }
    persist_tokens(&pending.home, &tokens)
}

pub(crate) async fn exchange_tokens(
    client: &reqwest::Client,
    token_url: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<OAuthTokenResponse> {
    let body = serde_urlencoded::to_string([
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", redirect_uri),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ])
    .context("encode oauth form")?;
    let response = client
        .post(token_url)
        .header("User-Agent", USER_AGENT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("无法连接授权服务，请稍后重新登录"))?;
    if !response.status().is_success() {
        let status = response.status();
        bail!("Token 交换失败: {status}，请重新登录");
    }
    response
        .json()
        .await
        .map_err(|_| anyhow::anyhow!("授权服务返回了无效的 Token 响应"))
}

pub async fn exchange_refresh_token(
    client: &reqwest::Client,
    refresh_token: &str,
) -> Result<OAuthTokenResponse> {
    exchange_refresh_token_at(client, OAUTH_TOKEN_URL, refresh_token).await
}

async fn exchange_refresh_token_at(
    client: &reqwest::Client,
    token_url: &str,
    refresh_token: &str,
) -> Result<OAuthTokenResponse> {
    let refresh_token = refresh_token.trim();
    if refresh_token.is_empty() {
        bail!("请填写 refresh_token");
    }
    let response = client
        .post(token_url)
        .header("User-Agent", USER_AGENT)
        .header("Originator", "codex_cli_rs")
        .json(&json!({
            "client_id": CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
        }))
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("无法连接授权服务，请检查网络后重试"))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = oauth_error_detail(&body, &[refresh_token])
            .map(|message| format!(" - {message}"))
            .unwrap_or_default();
        bail!("Refresh Token 换取失败: {status}{detail}");
    }
    serde_json::from_str(&body).map_err(|_| anyhow::anyhow!("授权服务返回了无效的 Token 响应"))
}

fn stored_refresh_token(home: &Path) -> Option<String> {
    let logged_in = |path: PathBuf| {
        read_auth_file(&path)
            .ok()
            .flatten()
            .filter(|auth| status_from_auth(auth).logged_in)
    };
    let kit = logged_in(kit_auth_path(home));
    let official = logged_in(official_auth_path(home));
    let auth = match (kit, official) {
        (Some(kit_auth), Some(official_auth))
            if should_adopt_official_refresh(&kit_auth, &official_auth) =>
        {
            official_auth
        }
        (Some(kit_auth), _) => kit_auth,
        (None, Some(official_auth)) => official_auth,
        (None, None) => return None,
    };
    auth.get("tokens")
        .and_then(Value::as_object)
        .and_then(|tokens| tokens.get("refresh_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn refresh_can_reuse_current(message: &str) -> bool {
    message.contains("earliest_refresh_at") || message.contains(" 429")
}

/// 用当前登录文件里的 RT 换新 AT/RT，立刻写回 Kit 与接入中的官方登录文件。
pub(crate) async fn refresh_session_credentials(
    home: &Path,
    client: &reqwest::Client,
    token_url: &str,
) -> Result<ChatGptCredentials> {
    let creds = chatgpt_credentials(home)?;
    if !creds.refreshable {
        return Ok(creds);
    }
    let Some(refresh_token) = stored_refresh_token(home) else {
        return Ok(creds);
    };
    match exchange_refresh_token_at(client, token_url, &refresh_token).await {
        Ok(tokens) => {
            persist_refresh_token_import(home, &refresh_token, &tokens)?;
            chatgpt_credentials(home)
        }
        Err(err) => {
            if refresh_can_reuse_current(&format!("{err:#}")) {
                return Ok(creds);
            }
            Err(err)
        }
    }
}

pub fn persist_refresh_token_import(
    home: &Path,
    supplied_refresh_token: &str,
    tokens: &OAuthTokenResponse,
) -> Result<LoginStatus> {
    let access_token = tokens.access_token.trim();
    if access_token.is_empty() {
        bail!("刷新响应缺少 access_token");
    }
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| supplied_refresh_token.trim());
    if refresh_token.is_empty() {
        bail!("刷新响应缺少 refresh_token");
    }

    // Codex 原生 auth.json 要求 id_token 为可解析 JWT。部分刷新响应不返回
    // id_token，此时沿用官方外部 AT 登录的做法，用 access token 提供身份声明。
    let id_token = tokens
        .id_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .filter(|value| parse_jwt_claims(value).is_some())
        .or_else(|| {
            parse_jwt_claims(access_token)
                .is_some()
                .then_some(access_token)
        })
        .ok_or_else(|| {
            anyhow::anyhow!("无法从换取结果解析账号身份，请确认 RT 属于 Codex/ChatGPT 登录")
        })?;
    let (account_id, email) = extract_account_metadata(id_token, access_token);
    let account_id =
        account_id.ok_or_else(|| anyhow::anyhow!("无法从 token 中提取 chatgpt_account_id"))?;
    write_imported_refresh_auth(home, id_token, access_token, refresh_token, &account_id)?;
    overlay_kit_onto_official(home)?;
    Ok(LoginStatus {
        logged_in: true,
        auth_mode: Some("chatgpt".into()),
        email,
        account_id: Some(account_id),
        refreshable: true,
    })
}

pub fn import_access_token(home: &Path, access_token: &str) -> Result<LoginStatus> {
    let access_token = access_token.trim();
    if access_token.is_empty() {
        bail!("请填写 access_token");
    }
    if parse_jwt_claims(access_token).is_none() {
        bail!("Access Token 不是可解析的 Codex JWT");
    }
    let (account_id, email) = extract_account_metadata(access_token, access_token);
    let account_id =
        account_id.ok_or_else(|| anyhow::anyhow!("无法从 Access Token 提取 chatgpt_account_id"))?;
    write_access_token_auth(home, access_token, &account_id)?;
    overlay_kit_onto_official(home)?;
    Ok(LoginStatus {
        logged_in: true,
        auth_mode: Some("chatgptAuthTokens".into()),
        email,
        account_id: Some(account_id),
        refreshable: false,
    })
}

fn oauth_error_detail(body: &str, secrets: &[&str]) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let mut message = ["error_description", "error", "message"]
        .into_iter()
        .find_map(|key| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or("")
        .to_string();
    if let Some(at) = value
        .get("earliest_refresh_at")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if message.is_empty() {
            message = format!("earliest_refresh_at={at}");
        } else {
            message = format!("{message}; earliest_refresh_at={at}");
        }
    }
    if message.is_empty() {
        return None;
    }
    for secret in secrets {
        let secret = secret.trim();
        if !secret.is_empty() {
            message = message.replace(secret, "[redacted]");
        }
    }
    Some(message.chars().take(500).collect())
}

pub(crate) fn persist_tokens(home: &Path, tokens: &OAuthTokenResponse) -> Result<LoginStatus> {
    if tokens.access_token.trim().is_empty() {
        bail!("登录响应缺少 access_token");
    }
    let refresh_token = tokens
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("登录响应缺少 refresh_token"))?;
    let id_token = tokens
        .id_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("登录响应缺少 id_token"))?;
    let (account_id, email) = extract_account_metadata(id_token, &tokens.access_token);
    let account_id =
        account_id.ok_or_else(|| anyhow::anyhow!("无法从 token 中提取 chatgpt_account_id"))?;
    write_session_auth(
        home,
        id_token,
        &tokens.access_token,
        refresh_token,
        &account_id,
    )?;
    overlay_kit_onto_official(home)?;
    Ok(LoginStatus {
        logged_in: true,
        auth_mode: Some("chatgpt".into()),
        email,
        account_id: Some(account_id),
        refreshable: true,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoredAuthMode {
    Managed,
    ExternalAccessToken,
}

impl StoredAuthMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Managed => "chatgpt",
            Self::ExternalAccessToken => "chatgptAuthTokens",
        }
    }
}

fn write_session_auth(
    home: &Path,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
    account_id: &str,
) -> Result<()> {
    remember_accounts_around(home, || {
        write_auth_json(
            &kit_auth_path(home),
            id_token,
            access_token,
            refresh_token,
            account_id,
            StoredAuthMode::Managed,
            false,
        )
    })
}

fn write_imported_refresh_auth(
    home: &Path,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
    account_id: &str,
) -> Result<()> {
    remember_accounts_around(home, || {
        write_auth_json(
            &kit_auth_path(home),
            id_token,
            access_token,
            refresh_token,
            account_id,
            StoredAuthMode::Managed,
            true,
        )
    })
}

fn write_access_token_auth(home: &Path, access_token: &str, account_id: &str) -> Result<()> {
    // 与 Codex 的 external access token 结构一致：AT 同时提供 JWT 身份声明，
    // refresh_token 保留为空字符串，auth_mode 标记为 chatgptAuthTokens。
    remember_accounts_around(home, || {
        write_auth_json(
            &kit_auth_path(home),
            access_token,
            access_token,
            "",
            account_id,
            StoredAuthMode::ExternalAccessToken,
            true,
        )
    })
}

/// Saves the outgoing Kit login into the account vault before a new login
/// replaces it, and the new login afterwards, so no account is lost.
fn remember_accounts_around(home: &Path, write: impl FnOnce() -> Result<()>) -> Result<()> {
    crate::accounts::capture_quietly(home);
    write()?;
    crate::accounts::capture_quietly(home);
    Ok(())
}

fn write_auth_json(
    path: &Path,
    id_token: &str,
    access_token: &str,
    refresh_token: &str,
    account_id: &str,
    auth_mode: StoredAuthMode,
    replace_tokens: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut auth = match read_auth_file(path)? {
        Some(Value::Object(map)) => Value::Object(map),
        _ => json!({}),
    };

    let previous_account = auth
        .get("tokens")
        .and_then(Value::as_object)
        .and_then(|tokens| tokens.get("account_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let account_changed = previous_account.as_deref() != Some(account_id);

    let obj = auth
        .as_object_mut()
        .expect("auth.json root must be an object");
    obj.insert("auth_mode".into(), json!(auth_mode.as_str()));
    obj.insert("OPENAI_API_KEY".into(), Value::Null);
    obj.remove("personal_access_token");
    obj.remove("bedrock_api_key");
    obj.remove("bedrock_access_keys");
    obj.insert(
        "last_refresh".into(),
        json!(Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)),
    );
    if account_changed || auth_mode == StoredAuthMode::ExternalAccessToken {
        // 官方桌面端会在 auth.json 里写入 agent_identity 等账号专属字段，
        // 切号或改为外部 AT 后必须丢掉。
        obj.remove("agent_identity");
    }

    let mut tokens = if account_changed || replace_tokens {
        serde_json::Map::new()
    } else {
        obj.get("tokens")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    tokens.insert("id_token".into(), json!(id_token));
    tokens.insert("access_token".into(), json!(access_token));
    tokens.insert("refresh_token".into(), json!(refresh_token));
    tokens.insert("account_id".into(), json!(account_id));
    obj.insert("tokens".into(), Value::Object(tokens));

    atomic_write(path, &serde_json::to_vec_pretty(&auth)?)
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("auth.json")
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    Ok(())
}

pub(crate) fn read_auth_file(path: &Path) -> Result<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw).context("parse auth file")?;
    Ok(Some(value))
}

pub(crate) fn status_from_auth(auth: &Value) -> LoginStatus {
    let tokens = auth.get("tokens").and_then(Value::as_object);
    let access = tokens
        .and_then(|map| map.get("access_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let refresh = tokens
        .and_then(|map| map.get("refresh_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let id_token = tokens
        .and_then(|map| map.get("id_token"))
        .and_then(Value::as_str);
    let stored_account = tokens
        .and_then(|map| map.get("account_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let (jwt_account, email) =
        extract_account_metadata(id_token.unwrap_or(""), access.unwrap_or(""));
    let account_id = jwt_account.or(stored_account);
    let explicit_auth_mode = auth
        .get("auth_mode")
        .and_then(Value::as_str)
        .map(str::to_string);
    let resolved_mode = resolved_auth_mode(auth);
    let logged_in = access.is_some()
        && account_id.is_some()
        && match resolved_mode {
            ResolvedAuthMode::Chatgpt => refresh.is_some(),
            ResolvedAuthMode::ChatgptAuthTokens => true,
            ResolvedAuthMode::ApiKey | ResolvedAuthMode::Other => false,
        };
    LoginStatus {
        logged_in,
        auth_mode: explicit_auth_mode.or_else(|| logged_in.then(|| "chatgpt".into())),
        email,
        account_id,
        refreshable: logged_in && resolved_mode == ResolvedAuthMode::Chatgpt && refresh.is_some(),
    }
}

fn resolved_auth_mode(auth: &Value) -> ResolvedAuthMode {
    let Some(obj) = auth.as_object() else {
        return ResolvedAuthMode::Other;
    };
    let present = |key: &str| obj.get(key).is_some_and(|value| !value.is_null());
    if let Some(mode) = obj.get("auth_mode").and_then(Value::as_str) {
        return match mode {
            "chatgpt" => ResolvedAuthMode::Chatgpt,
            "chatgptAuthTokens" => ResolvedAuthMode::ChatgptAuthTokens,
            "apikey" => ResolvedAuthMode::ApiKey,
            _ => ResolvedAuthMode::Other,
        };
    }
    if present("personal_access_token")
        || present("bedrock_api_key")
        || present("bedrock_access_keys")
        || present("OPENAI_API_KEY")
    {
        return ResolvedAuthMode::ApiKey;
    }
    ResolvedAuthMode::Chatgpt
}

fn extract_account_metadata(
    id_token: &str,
    access_token: &str,
) -> (Option<String>, Option<String>) {
    let mut account_id = None;
    let mut email = None;
    if let Some(claims) = parse_jwt_claims(id_token) {
        account_id = chatgpt_account_id(&claims);
        email = claims
            .get("email")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
    }
    if account_id.is_none() || email.is_none() {
        if let Some(claims) = parse_jwt_claims(access_token) {
            account_id = account_id.or_else(|| chatgpt_account_id(&claims));
            if email.is_none() {
                email = claims
                    .get("email")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
            }
        }
    }
    (account_id, email)
}

fn chatgpt_account_id(claims: &Value) -> Option<String> {
    claims
        .get("chatgpt_account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            claims
                .get("https://api.openai.com/auth")
                .and_then(Value::as_object)
                .and_then(|map| map.get("chatgpt_account_id"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

fn parse_jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&decoded).ok()
}

fn parse_interval(value: Option<&Value>) -> u64 {
    let parsed = match value {
        Some(Value::Number(number)) => number
            .as_u64()
            .or_else(|| number.as_f64().map(|float| float as u64)),
        Some(Value::String(text)) => text.parse().ok(),
        _ => None,
    };
    parsed.unwrap_or(DEFAULT_INTERVAL).max(1)
}

fn is_access_denied(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    lower.contains("access_denied") || lower.contains("access denied")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};
    use http::{HeaderMap, HeaderValue};
    use serde_json::json;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use tokio::net::TcpListener;

    #[derive(Clone, Copy)]
    enum MockMode {
        Success,
        Pending,
        MissingIdToken,
    }

    #[derive(Clone)]
    struct MockState {
        mode: MockMode,
        polls: Arc<AtomicU32>,
    }

    fn test_jwt(account: &str, email: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            json!({
                "chatgpt_account_id": account,
                "email": email,
            })
            .to_string(),
        );
        format!("{header}.{payload}.sig")
    }

    async fn usercode() -> impl IntoResponse {
        Json(json!({
            "device_auth_id": "device-1",
            "user_code": "ABCD-EFGH",
            "expires_in": 900,
            "interval": 1,
        }))
    }

    async fn poll_token(State(state): State<MockState>) -> impl IntoResponse {
        let count = state.polls.fetch_add(1, Ordering::SeqCst);
        match state.mode {
            MockMode::Pending => (StatusCode::FORBIDDEN, "pending").into_response(),
            MockMode::Success | MockMode::MissingIdToken if count == 0 => {
                (StatusCode::FORBIDDEN, "pending").into_response()
            }
            _ => Json(json!({
                "authorization_code": "code-1",
                "code_verifier": "verifier-1",
            }))
            .into_response(),
        }
    }

    async fn oauth_token(State(state): State<MockState>) -> impl IntoResponse {
        let id_token = match state.mode {
            MockMode::MissingIdToken => None,
            _ => Some(test_jwt("acct-1", "user@example.com")),
        };
        Json(json!({
            "access_token": "access-1",
            "refresh_token": "refresh-1",
            "id_token": id_token,
        }))
    }

    async fn serve(mode: MockMode) -> (SocketAddr, LoginEndpoints) {
        let state = MockState {
            mode,
            polls: Arc::new(AtomicU32::new(0)),
        };
        let app = Router::new()
            .route("/usercode", post(usercode))
            .route("/token", post(poll_token))
            .route("/oauth/token", post(oauth_token))
            .with_state(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://{addr}");
        (
            addr,
            LoginEndpoints {
                usercode_url: format!("{base}/usercode"),
                poll_url: format!("{base}/token"),
                oauth_token_url: format!("{base}/oauth/token"),
            },
        )
    }

    async fn serve_refresh_response(
        status: StatusCode,
        response: Value,
    ) -> (String, tokio::sync::mpsc::Receiver<Value>) {
        let (send, receive) = tokio::sync::mpsc::channel(1);
        let app = Router::new().route(
            "/oauth/token",
            post(move |Json(payload): Json<Value>| {
                let send = send.clone();
                let response = response.clone();
                async move {
                    send.send(payload).await.unwrap();
                    (status, Json(response))
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/oauth/token", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (url, receive)
    }

    #[tokio::test]
    async fn refresh_token_import_rotates_and_writes_native_auth() {
        let home = tempfile::tempdir().unwrap();
        let access_token = test_jwt("acct-rt", "rt@example.com");
        let (url, mut requests) = serve_refresh_response(
            StatusCode::OK,
            json!({
                "access_token": access_token,
                "refresh_token": "rotated-refresh"
            }),
        )
        .await;
        let client = token_import_http_client().unwrap();
        let tokens = exchange_refresh_token_at(&client, &url, " supplied-refresh ")
            .await
            .unwrap();
        let request = requests.recv().await.unwrap();
        assert_eq!(request["grant_type"], "refresh_token");
        assert_eq!(request["refresh_token"], "supplied-refresh");
        assert_eq!(request["client_id"], CLIENT_ID);

        let status =
            persist_refresh_token_import(home.path(), "supplied-refresh", &tokens).unwrap();
        assert!(status.logged_in);
        assert!(status.refreshable);
        assert_eq!(status.account_id.as_deref(), Some("acct-rt"));
        assert_eq!(status.email.as_deref(), Some("rt@example.com"));

        let raw = std::fs::read_to_string(kit_auth_path(home.path())).unwrap();
        let auth: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(auth["auth_mode"], "chatgpt");
        assert_eq!(auth["tokens"]["access_token"], access_token);
        assert_eq!(auth["tokens"]["id_token"], access_token);
        assert_eq!(auth["tokens"]["refresh_token"], "rotated-refresh");
        assert_eq!(auth["tokens"]["account_id"], "acct-rt");
        assert_eq!(
            std::fs::read_to_string(home.path().join("auth.json")).unwrap(),
            raw
        );
    }

    #[test]
    fn refresh_token_import_keeps_supplied_token_when_server_does_not_rotate() {
        let home = tempfile::tempdir().unwrap();
        let access_token = test_jwt("acct-rt-fallback", "fallback@example.com");
        let status = persist_refresh_token_import(
            home.path(),
            "original-refresh",
            &OAuthTokenResponse {
                access_token,
                refresh_token: None,
                id_token: None,
            },
        )
        .unwrap();
        assert!(status.refreshable);
        let auth: Value =
            serde_json::from_str(&std::fs::read_to_string(kit_auth_path(home.path())).unwrap())
                .unwrap();
        assert_eq!(auth["tokens"]["refresh_token"], "original-refresh");
    }

    #[tokio::test]
    async fn refresh_session_credentials_rotates_and_rewrites_login_files() {
        let home = tempfile::tempdir().unwrap();
        let old_access = test_jwt("acct-rt", "old@example.com");
        let new_access = test_jwt("acct-rt", "new@example.com");
        std::fs::write(
            kit_auth_path(home.path()),
            json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": old_access,
                    "access_token": old_access,
                    "refresh_token": "stored-refresh",
                    "account_id": "acct-rt"
                }
            })
            .to_string(),
        )
        .unwrap();
        let (url, mut requests) = serve_refresh_response(
            StatusCode::OK,
            json!({
                "access_token": new_access,
                "refresh_token": "rotated-session-refresh"
            }),
        )
        .await;
        let client = token_import_http_client().unwrap();
        let creds = refresh_session_credentials(home.path(), &client, &url)
            .await
            .unwrap();
        let request = requests.recv().await.unwrap();
        assert_eq!(request["refresh_token"], "stored-refresh");
        assert_eq!(creds.access_token, new_access);
        assert_eq!(creds.account_id, "acct-rt");
        assert!(creds.refreshable);

        let kit: Value =
            serde_json::from_str(&std::fs::read_to_string(kit_auth_path(home.path())).unwrap())
                .unwrap();
        assert_eq!(kit["tokens"]["access_token"], new_access);
        assert_eq!(kit["tokens"]["refresh_token"], "rotated-session-refresh");
        assert_eq!(
            std::fs::read_to_string(home.path().join("auth.json")).unwrap(),
            std::fs::read_to_string(kit_auth_path(home.path())).unwrap()
        );
    }

    #[tokio::test]
    async fn refresh_session_credentials_keeps_current_when_too_soon() {
        let home = tempfile::tempdir().unwrap();
        let access = test_jwt("acct-rt", "soon@example.com");
        std::fs::write(
            kit_auth_path(home.path()),
            json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": access,
                    "access_token": access,
                    "refresh_token": "stored-refresh",
                    "account_id": "acct-rt"
                }
            })
            .to_string(),
        )
        .unwrap();
        let (url, _requests) = serve_refresh_response(
            StatusCode::BAD_REQUEST,
            json!({"error":"invalid_request","earliest_refresh_at":"2026-09-30T00:00:00Z"}),
        )
        .await;
        let client = token_import_http_client().unwrap();
        let creds = refresh_session_credentials(home.path(), &client, &url)
            .await
            .unwrap();
        assert_eq!(creds.access_token, access);
        let kit: Value =
            serde_json::from_str(&std::fs::read_to_string(kit_auth_path(home.path())).unwrap())
                .unwrap();
        assert_eq!(kit["tokens"]["refresh_token"], "stored-refresh");
    }

    #[tokio::test]
    async fn refresh_token_error_redacts_supplied_secret() {
        let (url, _requests) = serve_refresh_response(
            StatusCode::BAD_REQUEST,
            json!({"error_description": "rejected supplied-refresh"}),
        )
        .await;
        let client = token_import_http_client().unwrap();
        let error = exchange_refresh_token_at(&client, &url, "supplied-refresh")
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("supplied-refresh"));
        assert!(error.contains("[redacted]"));
    }

    #[test]
    fn access_token_import_writes_external_native_auth_without_old_refresh_token() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            kit_auth_path(home.path()),
            r#"{
  "auth_mode": "chatgpt",
  "agent_identity": "old-identity",
  "tokens": {
    "id_token": "old-id",
    "access_token": "old-access",
    "refresh_token": "old-refresh",
    "account_id": "acct-at",
    "extra_flag": true
  }
}"#,
        )
        .unwrap();
        let access_token = test_jwt("acct-at", "at@example.com");
        let status = import_access_token(home.path(), &access_token).unwrap();
        assert!(status.logged_in);
        assert!(!status.refreshable);
        assert_eq!(status.auth_mode.as_deref(), Some("chatgptAuthTokens"));
        assert_eq!(status.account_id.as_deref(), Some("acct-at"));

        let raw = std::fs::read_to_string(kit_auth_path(home.path())).unwrap();
        let auth: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(auth["auth_mode"], "chatgptAuthTokens");
        assert_eq!(auth["tokens"]["id_token"], access_token);
        assert_eq!(auth["tokens"]["access_token"], access_token);
        assert_eq!(auth["tokens"]["refresh_token"], "");
        assert_eq!(auth["tokens"]["account_id"], "acct-at");
        assert!(auth["tokens"].get("extra_flag").is_none());
        assert!(auth.get("agent_identity").is_none());
        assert!(login_status(home.path()).logged_in);
        assert!(!login_status(home.path()).refreshable);
        assert_eq!(
            chatgpt_credentials(home.path()).unwrap().access_token,
            access_token
        );
    }

    #[test]
    fn invalid_access_token_does_not_replace_existing_auth() {
        let home = tempfile::tempdir().unwrap();
        let existing = r#"{"auth_mode":"chatgpt","tokens":{"id_token":"old","access_token":"old","refresh_token":"old-refresh","account_id":"old-account"}}"#;
        std::fs::write(kit_auth_path(home.path()), existing).unwrap();
        assert!(import_access_token(home.path(), "not-a-jwt").is_err());
        assert_eq!(
            std::fs::read_to_string(kit_auth_path(home.path())).unwrap(),
            existing
        );
        assert!(!home.path().join("auth.json").exists());
    }

    #[tokio::test]
    async fn cancelled_device_session_cannot_write_auth() {
        let home = tempfile::tempdir().unwrap();
        let (_addr, endpoints) = serve(MockMode::Success).await;
        let client = http_client().unwrap();
        let (_, pending) = start_device_login(&client, &endpoints, home.path().into())
            .await
            .unwrap();
        pending.cancel();
        assert!(poll_device_login(&client, &endpoints, &pending)
            .await
            .is_err());
        assert!(!home.path().join("auth.json").exists());
        assert!(!kit_auth_path(home.path()).exists());
    }

    #[tokio::test]
    async fn pending_403_does_not_write_auth() {
        let home = tempfile::tempdir().unwrap();
        let (_addr, endpoints) = serve(MockMode::Pending).await;
        let client = http_client().unwrap();
        let (_start, pending) = start_device_login(&client, &endpoints, home.path().to_path_buf())
            .await
            .unwrap();
        let poll = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        assert_eq!(poll.status, PollStatus::Pending);
        assert!(!home.path().join("auth.json").exists());
        assert!(!kit_auth_path(home.path()).exists());
    }

    #[tokio::test]
    async fn success_writes_native_auth_json() {
        let home = tempfile::tempdir().unwrap();
        let (_addr, endpoints) = serve(MockMode::Success).await;
        let client = http_client().unwrap();
        let (start, pending) = start_device_login(&client, &endpoints, home.path().to_path_buf())
            .await
            .unwrap();
        assert_eq!(start.user_code, "ABCD-EFGH");
        let first = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        assert_eq!(first.status, PollStatus::Pending);
        let second = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        assert_eq!(second.status, PollStatus::Ok);
        let raw = std::fs::read_to_string(kit_auth_path(home.path())).unwrap();
        assert_eq!(
            std::fs::read_to_string(home.path().join("auth.json")).unwrap(),
            raw
        );
        assert_eq!(
            std::fs::metadata(official_auth_backup_path(home.path()))
                .unwrap()
                .len(),
            0
        );
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["auth_mode"], "chatgpt");
        assert!(value.get("OPENAI_API_KEY").unwrap().is_null());
        assert_eq!(value["tokens"]["access_token"], "access-1");
        assert_eq!(value["tokens"]["refresh_token"], "refresh-1");
        assert_eq!(value["tokens"]["account_id"], "acct-1");
        assert!(value["tokens"]["id_token"].as_str().unwrap().contains('.'));
        assert!(value["last_refresh"].as_str().unwrap().ends_with('Z'));
        let status = login_status(home.path());
        assert!(status.logged_in);
        assert_eq!(status.email.as_deref(), Some("user@example.com"));
        assert_eq!(status.account_id.as_deref(), Some("acct-1"));
        let encoded = serde_json::to_value(&status).unwrap();
        assert!(encoded.get("accessToken").is_none());
        assert!(encoded.get("refreshToken").is_none());
        assert!(encoded.get("idToken").is_none());
        assert!(encoded.get("tokens").is_none());
    }

    #[tokio::test]
    async fn missing_id_token_does_not_write_auth() {
        let home = tempfile::tempdir().unwrap();
        let (_addr, endpoints) = serve(MockMode::MissingIdToken).await;
        let client = http_client().unwrap();
        let (_start, pending) = start_device_login(&client, &endpoints, home.path().to_path_buf())
            .await
            .unwrap();
        let _ = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        let poll = poll_device_login(&client, &endpoints, &pending)
            .await
            .unwrap();
        assert_eq!(poll.status, PollStatus::Failed);
        assert!(!home.path().join("auth.json").exists());
        assert!(!kit_auth_path(home.path()).exists());
    }

    #[test]
    fn credentials_come_from_native_auth() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            r#"{
  "auth_mode": "chatgpt",
  "tokens": {
    "id_token": "a.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2N0In0.sig",
    "access_token": "access",
    "refresh_token": "refresh",
    "account_id": "acct"
  }
}"#,
        )
        .unwrap();
        let creds = chatgpt_credentials(home.path()).unwrap();
        assert_eq!(creds.access_token, "access");
        assert_eq!(creds.account_id, "acct");
        let (_, override_headers) = request_credentials(home.path()).unwrap();
        assert!(!override_headers);
    }

    #[test]
    fn newer_same_account_official_refresh_is_promoted_to_kit() {
        let home = tempfile::tempdir().unwrap();
        let old_access = test_jwt("acct-sync", "sync@example.com");
        let new_access = test_jwt("acct-sync", "sync@example.com");
        std::fs::write(
            kit_auth_path(home.path()),
            serde_json::to_vec_pretty(&json!({
                "auth_mode": "chatgpt",
                "last_refresh": "2026-09-20T01:00:00Z",
                "tokens": {
                    "id_token": old_access,
                    "access_token": "old-access",
                    "refresh_token": "old-refresh",
                    "account_id": "acct-sync"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            serde_json::to_vec_pretty(&json!({
                "auth_mode": "chatgpt",
                "last_refresh": "2026-09-20T02:00:00Z",
                "tokens": {
                    "id_token": new_access,
                    "access_token": "new-access",
                    "refresh_token": "rotated-refresh",
                    "account_id": "acct-sync"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let (credentials, override_headers) = request_credentials(home.path()).unwrap();
        assert!(override_headers);
        assert_eq!(credentials.access_token, "new-access");
        let synced: Value =
            serde_json::from_str(&std::fs::read_to_string(kit_auth_path(home.path())).unwrap())
                .unwrap();
        assert_eq!(synced["tokens"]["refresh_token"], "rotated-refresh");
    }

    #[test]
    fn newer_other_account_official_refresh_is_not_promoted() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            kit_auth_path(home.path()),
            serde_json::to_vec_pretty(&json!({
                "auth_mode": "chatgpt",
                "last_refresh": "2026-09-20T01:00:00Z",
                "tokens": {
                    "id_token": test_jwt("kit-account", "kit@example.com"),
                    "access_token": "kit-access",
                    "refresh_token": "kit-refresh",
                    "account_id": "kit-account"
                }
            }))
            .unwrap(),
        )
        .unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            serde_json::to_vec_pretty(&json!({
                "auth_mode": "chatgpt",
                "last_refresh": "2026-09-20T02:00:00Z",
                "tokens": {
                    "id_token": test_jwt("other-account", "other@example.com"),
                    "access_token": "other-access",
                    "refresh_token": "other-refresh",
                    "account_id": "other-account"
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let (credentials, override_headers) = request_credentials(home.path()).unwrap();
        assert!(override_headers);
        assert_eq!(credentials.account_id, "kit-account");
        assert_eq!(credentials.access_token, "kit-access");
        let raw = std::fs::read_to_string(kit_auth_path(home.path())).unwrap();
        assert!(!raw.contains("other-access"));
    }

    #[test]
    fn api_key_only_is_not_chatgpt_login() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"sk-test"}"#,
        )
        .unwrap();
        assert!(!has_chatgpt_login(home.path()));
        let status = login_status(home.path());
        assert!(!status.logged_in);
        let encoded = serde_json::to_value(&status).unwrap();
        assert!(encoded.get("OPENAI_API_KEY").is_none());
    }

    #[test]
    fn write_session_auth_merges_extra_fields_without_touching_official() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(home.path().join("auth.json"), r#"{"auth_mode":"chatgpt","tokens":{"access_token":"official","refresh_token":"keep","account_id":"official-acct"}}"#).unwrap();
        std::fs::write(
            kit_auth_path(home.path()),
            r#"{
  "auth_mode": "chatgpt",
  "agent_identity": "keep-me",
  "tokens": {
    "id_token": "old-id",
    "access_token": "old-access",
    "refresh_token": "old-refresh",
    "account_id": "acct-1",
    "extra_flag": true
  }
}"#,
        )
        .unwrap();
        write_session_auth(home.path(), "new-id", "new-access", "new-refresh", "acct-1").unwrap();
        let official = std::fs::read_to_string(home.path().join("auth.json")).unwrap();
        assert!(official.contains("official-acct"));
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(kit_auth_path(home.path())).unwrap())
                .unwrap();
        assert_eq!(value["agent_identity"], "keep-me");
        assert_eq!(value["tokens"]["extra_flag"], true);
        assert_eq!(value["tokens"]["access_token"], "new-access");
        assert_eq!(value["tokens"]["account_id"], "acct-1");
    }

    fn fake_id_token(account: &str) -> String {
        let payload = URL_SAFE_NO_PAD.encode(format!(
            r#"{{"chatgpt_account_id":"{account}","email":"user@example.com"}}"#
        ));
        format!("hdr.{payload}.sig")
    }

    #[tokio::test]
    async fn auth_requests_use_the_accounts_outbound_line() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = proxy.local_addr().unwrap().port();
        let seen = tokio::spawn(async move {
            let (mut socket, _) = proxy.accept().await.unwrap();
            let mut buffer = vec![0u8; 1024];
            let read = socket.read(&mut buffer).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&buffer[..read]).to_string()
        });
        let client = http_client_via(&format!("http://127.0.0.1:{port}")).unwrap();
        client
            .get("http://auth.example.test/oauth/token")
            .send()
            .await
            .unwrap();
        let request = seen.await.unwrap();
        assert!(
            request.starts_with("GET http://auth.example.test/oauth/token HTTP/1.1"),
            "{request}"
        );
    }

    #[test]
    fn persist_tokens_overlays_official_and_restore_brings_it_back() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            r#"{"auth_mode":"chatgpt","tokens":{"access_token":"official","refresh_token":"keep","account_id":"official-acct"}}"#,
        )
        .unwrap();
        persist_tokens(
            home.path(),
            &OAuthTokenResponse {
                access_token: "kit-access".into(),
                refresh_token: Some("kit-refresh".into()),
                id_token: Some(fake_id_token("kit-acct")),
            },
        )
        .unwrap();
        let official = std::fs::read_to_string(home.path().join("auth.json")).unwrap();
        assert!(official.contains("kit-acct"));
        assert!(!official.contains("official-acct"));
        let bak = std::fs::read_to_string(official_auth_backup_path(home.path())).unwrap();
        assert!(bak.contains("official-acct"));
        restore_official_auth(home.path()).unwrap();
        let restored = std::fs::read_to_string(home.path().join("auth.json")).unwrap();
        assert!(restored.contains("official-acct"));
        assert!(!official_auth_backup_path(home.path()).exists());
    }

    #[test]
    fn write_session_auth_drops_old_account_identity() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            kit_auth_path(home.path()),
            r#"{
  "auth_mode": "chatgpt",
  "agent_identity": "old-account-identity",
  "tokens": {
    "id_token": "old-id",
    "access_token": "old-access",
    "refresh_token": "old-refresh",
    "account_id": "acct-old",
    "extra_flag": true
  }
}"#,
        )
        .unwrap();
        write_session_auth(
            home.path(),
            "new-id",
            "new-access",
            "new-refresh",
            "acct-new",
        )
        .unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(kit_auth_path(home.path())).unwrap())
                .unwrap();
        assert!(value.get("agent_identity").is_none());
        assert!(value["tokens"].get("extra_flag").is_none());
        assert_eq!(value["tokens"]["account_id"], "acct-new");
        assert_eq!(value["tokens"]["access_token"], "new-access");
    }

    #[test]
    fn same_account_stale_bearer_is_not_a_conflict() {
        let creds = ChatGptCredentials {
            access_token: "kit-access".into(),
            account_id: "acct-a".into(),
            email: None,
            auth_mode: "chatgpt".into(),
            refreshable: true,
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer stale-access"),
        );
        assert!(!credentials_conflict_headers(&headers, &creds));
        assert!(!credentials_match_headers(&headers, &creds));

        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("acct-a"),
        );
        assert!(!credentials_conflict_headers(&headers, &creds));
        assert!(credentials_match_headers(&headers, &creds));

        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("acct-b"),
        );
        assert!(credentials_conflict_headers(&headers, &creds));
        assert!(!credentials_match_headers(&headers, &creds));
    }

    #[test]
    fn kit_session_overrides_official_credentials() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("auth.json"),
            r#"{
  "auth_mode": "chatgpt",
  "tokens": {
    "id_token": "a.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJvZmZpY2lhbCJ9.sig",
    "access_token": "official-access",
    "refresh_token": "official-refresh",
    "account_id": "official"
  }
}"#,
        )
        .unwrap();
        std::fs::write(
            kit_auth_path(home.path()),
            r#"{
  "auth_mode": "chatgpt",
  "tokens": {
    "id_token": "a.eyJjaGF0Z3B0X2FjY291bnRfaWQiOiJraXQifQ.sig",
    "access_token": "kit-access",
    "refresh_token": "kit-refresh",
    "account_id": "kit"
  }
}"#,
        )
        .unwrap();
        let creds = chatgpt_credentials(home.path()).unwrap();
        assert_eq!(creds.access_token, "kit-access");
        assert_eq!(creds.account_id, "kit");
        let (request_creds, override_headers) = request_credentials(home.path()).unwrap();
        assert!(override_headers);
        assert_eq!(request_creds.account_id, "kit");
        assert_eq!(login_status(home.path()).account_id.as_deref(), Some("kit"));

        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer official-access"),
        );
        headers.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("kit"),
        );
        headers.insert(
            http::header::COOKIE,
            HeaderValue::from_static("session=old; __oailb=route1; chatgpt_session=nope"),
        );
        // Same account with a stale AT still matches; Kit will overwrite the Bearer.
        assert!(credentials_match_headers(&headers, &request_creds));
        assert!(!credentials_conflict_headers(&headers, &request_creds));
        apply_kit_auth_headers(&mut headers, home.path());
        assert!(credentials_match_headers(&headers, &request_creds));
        assert_eq!(
            headers.get(http::header::AUTHORIZATION).unwrap(),
            "Bearer kit-access"
        );
        assert_eq!(headers.get("chatgpt-account-id").unwrap(), "kit");
        assert_eq!(headers.get(http::header::COOKIE).unwrap(), "__oailb=route1");

        let mut session_only = HeaderMap::new();
        session_only.insert(
            http::header::AUTHORIZATION,
            HeaderValue::from_static("Bearer official-access"),
        );
        session_only.insert(
            HeaderName::from_static("chatgpt-account-id"),
            HeaderValue::from_static("kit"),
        );
        session_only.insert(
            http::header::COOKIE,
            HeaderValue::from_static("session=old"),
        );
        apply_kit_auth_headers(&mut session_only, home.path());
        assert!(session_only.get(http::header::COOKIE).is_none());
    }
}
