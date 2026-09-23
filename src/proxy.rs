use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode, Uri, Version};
use axum::response::{IntoResponse, Response};
use futures_util::{future::join_all, StreamExt};
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use url::Url;

use crate::attach::{self, is_attached};
use crate::chatgpt_cookies::{self, RoutingCookie};
use crate::diag;
use crate::fetch;
use crate::login::{self, has_chatgpt_login};
use crate::logs::{self, LogEntry, NetworkLogDetails, StreamLifecycle};
#[cfg(test)]
use crate::logs::ObservedStream;
use crate::traffic::{AccountTraffic, RequestActivity, TrafficTracker};
use crate::settings::{
    save_settings, NetworkRoutePolicy, OutboundMode, Settings, SettingsPatch, StateMissPolicy,
    TokenReusePolicy,
};
use crate::turn_state::{self, TurnStateStore, TurnStateView};
use crate::mihomo::{MihomoRuntime, MihomoStatus};
use crate::warp::{WarpRuntime, WarpStatus};

const TOKEN_FETCH_PAUSED_MESSAGE: &str = "已暂停获取 Token";
const TOKEN_MANUAL_REFRESH_MESSAGE: &str = "正在重新获取 Token…";
/// 已有数据块后，上游再静默这么久就切断，让 Codex 能报错重试。
const SSE_IDLE_AFTER_CHUNK: Duration = Duration::from_secs(90);
/// 响应头已到但还没有任何正文时，多等一会儿，避免误杀长思考。
const SSE_IDLE_BEFORE_CHUNK: Duration = Duration::from_secs(180);
const SSE_IDLE_TIMEOUT_EVENT: &[u8] = b"data: {\"type\":\"response.incomplete\"}\n\n";

fn business_stream_idle_timeout(chunks: u64) -> Duration {
    #[cfg(test)]
    if let Ok(ms) = std::env::var("CSK_SSE_IDLE_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            return Duration::from_millis(ms.max(1));
        }
    }
    if chunks == 0 {
        SSE_IDLE_BEFORE_CHUNK
    } else {
        SSE_IDLE_AFTER_CHUNK
    }
}

fn debug_log(msg: &str) {
    eprintln!("{}", msg);
    let path = crate::settings::home_dir().join(if cfg!(debug_assertions) {
        ".codex-state-kit-dev-debug.log"
    } else {
        ".codex-state-kit-debug.log"
    });
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let ts = chrono::Local::now().format("%H:%M:%S%.3f");
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FetchRetryClass {
    Normal,
    Backoff,
    Auth,
    Forbidden,
    Stale,
    Deferred,
}

#[derive(Debug)]
struct FetchOnceError {
    message: String,
    retry: FetchRetryClass,
    escalate: bool,
    returned_len: Option<usize>,
}

impl FetchOnceError {
    fn new(message: impl Into<String>, retry: FetchRetryClass) -> Self {
        Self {
            message: message.into(),
            retry,
            escalate: false,
            returned_len: None,
        }
    }

    fn probing(message: impl Into<String>, retry: FetchRetryClass, escalate: bool) -> Self {
        Self {
            message: message.into(),
            retry,
            escalate,
            returned_len: None,
        }
    }

    fn with_len(mut self, len: Option<usize>) -> Self {
        self.returned_len = len;
        self
    }
}

fn retry_priority(class: FetchRetryClass) -> u8 {
    match class {
        FetchRetryClass::Auth => 0,
        FetchRetryClass::Stale => 1,
        FetchRetryClass::Deferred => 2,
        FetchRetryClass::Backoff => 3,
        FetchRetryClass::Forbidden => 4,
        FetchRetryClass::Normal => 5,
    }
}

fn burst_key_for(store: &TurnStateStore, model: &str) -> String {
    if store.shares_292_for(model) {
        "shared_292".into()
    } else {
        model.to_string()
    }
}

fn fetch_failure_escalates(details: &NetworkLogDetails) -> bool {
    !matches!(details.response_status, Some(401 | 429 | 503))
}

/// 目标长度连续未命中并已打到最大并发时，返回静置时长。429/401/403 不走这条。
fn length_miss_rest(concurrency: usize, class: FetchRetryClass, escalate: bool) -> Option<Duration> {
    if escalate && class == FetchRetryClass::Normal && concurrency >= fetch::MAX_FETCH_BURST {
        Some(fetch::BURST_EXHAUSTED_BACKOFF)
    } else {
        None
    }
}

impl std::fmt::Display for FetchOnceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for FetchOnceError {}

fn fetch_retry_delay(class: FetchRetryClass) -> Duration {
    match class {
        FetchRetryClass::Normal => fetch::RETRY_INTERVAL,
        FetchRetryClass::Backoff => fetch::ERROR_BACKOFF,
        FetchRetryClass::Auth => fetch::AUTH_BACKOFF,
        FetchRetryClass::Forbidden => fetch::FORBIDDEN_BACKOFF,
        FetchRetryClass::Stale => fetch::RETRY_INTERVAL,
        FetchRetryClass::Deferred => fetch::RETRY_INTERVAL,
    }
}

fn classify_fetch_failure(details: &NetworkLogDetails) -> FetchRetryClass {
    match details.response_status {
        Some(401) => FetchRetryClass::Auth,
        Some(403) => FetchRetryClass::Forbidden,
        Some(429 | 503) => FetchRetryClass::Backoff,
        _ if details.error_kind.as_deref() == Some("connect") => FetchRetryClass::Backoff,
        _ => FetchRetryClass::Normal,
    }
}

fn model_for_fetch_round(models: &[String], round: u32) -> Option<&str> {
    if models.is_empty() {
        return None;
    }
    Some(models[(round.saturating_sub(1) as usize) % models.len()].as_str())
}

/// 指定了取票模型时，共享 292 只保留这一个供体；其他绑定长度仍各自刷新。
fn pin_shared_donor(store: &TurnStateStore, models: &[String], donor: &str) -> Vec<String> {
    let mut kept = Vec::new();
    let mut shared_due = false;
    let mut donor_kept = false;
    for model in models {
        if store.shares_292_for(model) {
            shared_due = true;
            continue;
        }
        if model == donor {
            donor_kept = true;
        }
        kept.push(model.clone());
    }
    if shared_due && !donor_kept {
        kept.insert(0, donor.to_string());
    }
    kept
}

/// Shared 292 is one refresh target. Pick any eligible donor at random, while
/// retaining round-robin fairness for independently bound non-292 models.
fn model_for_reuse_round<'a>(store: &TurnStateStore, models: &'a [String], round: u32) -> Option<&'a str> {
    let shared: Vec<&String> = models.iter().filter(|model| store.shares_292_for(model)).collect();
    if shared.is_empty() {
        return model_for_fetch_round(models, round);
    }
    let donor = shared[(rand::random::<u64>() % shared.len() as u64) as usize];
    let mut candidates = vec![donor.as_str()];
    candidates.extend(models.iter().filter(|model| !store.shares_292_for(model)).map(String::as_str));
    Some(candidates[(round.saturating_sub(1) as usize) % candidates.len()])
}

fn capture_fetched_ticket(
    store: &mut TurnStateStore,
    model: &str,
    token: &str,
    proxy_session: Option<&str>,
    previous_response_id: Option<&str>,
    routing_cookies: &[RoutingCookie],
) -> bool {
    if !store.capture_with_session(
        model,
        token,
        "fetch",
        proxy_session,
        previous_response_id,
        routing_cookies,
    ) {
        return false;
    }
    token.trim().len() == store.bound_len_for(model)
        && (store.shares_292_for(model)
            || store.peek_for_model(model).as_deref() == Some(token.trim()))
        && !store.needs_refresh(model)
}

fn degraded_response_model<'a>(
    request_model: Option<&'a str>,
    upstream_token: Option<&str>,
) -> Option<&'a str> {
    request_model.filter(|_| upstream_token.is_some_and(turn_state::is_degraded_token))
}

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub proxy_listen: String,
    pub upstream: String,
    pub codex_home: String,
    pub proxy_ok: bool,
    pub attached: bool,
    pub proxy_error: Option<String>,
    pub attach_error: Option<String>,
    pub outbound_proxy: String,
    pub upstream_proxy: String,
    pub outbound_mode: OutboundMode,
    pub warp_http2: bool,
    pub warp: WarpStatus,
    pub mihomo_subscription: String,
    pub mihomo_node: String,
    pub mihomo: MihomoStatus,
    pub fetch_error: Option<String>,
    pub fetch_ok_at: Option<String>,
    pub turn_state: TurnStateView,
    pub degraded: bool,
    pub degraded_at: Option<String>,
    pub logs: Vec<LogEntry>,
    pub account_traffic: AccountTraffic,
    pub current_account_id: Option<String>,
    pub current_account_email: Option<String>,
    pub state_miss_policy: StateMissPolicy,
    pub token_reuse_policy: TokenReusePolicy,
    pub state_fetch_model: String,
    pub network_route_policy: NetworkRoutePolicy,
    pub forced_model: String,
    pub configured_models: Vec<String>,
    pub token_fetch_paused: bool,
    pub token_max_age_mins: u32,
    pub token_prefetch_age_mins: u32,
    pub diag_log_path: String,
}

pub struct App {
    pub warp: WarpRuntime,
    pub mihomo: MihomoRuntime,
    pub settings: Mutex<Settings>,
    pub logs: Mutex<VecDeque<LogEntry>>,
    traffic: TrafficTracker,
    pub proxy_ok: AtomicBool,
    pub login_http: reqwest::Client,
    leftover_restored: AtomicBool,
    proxy_error: Mutex<Option<String>>,
    fetch_error: Mutex<Option<String>>,
    fetch_ok_at: Mutex<Option<String>>,
    fetch_round: AtomicU32,
    fetch_gate: Mutex<()>,
    fetch_next_allowed_at: Mutex<Instant>,
    fetch_model_next_allowed_at: Mutex<HashMap<String, Instant>>,
    fetch_burst_misses: Mutex<HashMap<String, u32>>,
    fetch_generation: AtomicU64,
    fetch_change_notify: Notify,
    fetch_transition: Mutex<()>,
    turn_state: Mutex<TurnStateStore>,
    http: Mutex<reqwest::Client>,
    degraded: AtomicBool,
    degraded_at: Mutex<Option<String>>,
    pub degrade_notify: Notify,
    pub warp_wake: Notify,
    /// 新模型被发现时通知 fetch 循环立即唤醒
    model_notify: Notify,
    /// 是否已注册 settings.models 中的种子模型
    seeds_registered: AtomicBool,
    #[cfg(test)]
    oauth_token_url: std::sync::Mutex<Option<String>>,
}

/// Callers hold fetch_transition while this guard is alive. Dropping a request
/// during an identity switch must also end the odd (transitioning) generation.
struct IdentityGenerationChange<'a>(&'a App);

impl<'a> IdentityGenerationChange<'a> {
    fn new(app: &'a App) -> Self {
        app.fetch_generation.fetch_add(1, Ordering::SeqCst);
        app.fetch_change_notify.notify_waiters();
        Self(app)
    }
}

impl Drop for IdentityGenerationChange<'_> {
    fn drop(&mut self) {
        self.0.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.0.fetch_change_notify.notify_waiters();
    }
}

impl App {
    pub fn new(settings: Settings) -> Result<Self> {
        Self::with_warp(settings, WarpRuntime::default())
    }

    pub fn with_warp(settings: Settings, warp: WarpRuntime) -> Result<Self> {
        Self::with_sidecars(settings, warp, MihomoRuntime::default())
    }

    pub fn with_sidecars(
        settings: Settings,
        warp: WarpRuntime,
        mihomo: MihomoRuntime,
    ) -> Result<Self> {
        let http = business_http_client(&resolved_business_proxy(&settings, &warp, &mihomo), None)?;
        let mut turn_state = TurnStateStore::load();
        turn_state.set_reuse_policy(settings.token_reuse_policy);
        turn_state.set_lifetime(turn_state::MAX_AGE_SECS, turn_state::PREFETCH_AGE_SECS);
        Ok(Self {
            warp,
            mihomo,
            settings: Mutex::new(settings),
            logs: Mutex::new(VecDeque::with_capacity(80)),
            traffic: TrafficTracker::default(),
            proxy_ok: AtomicBool::new(false),
            login_http: crate::login::http_client()?,
            leftover_restored: AtomicBool::new(false),
            proxy_error: Mutex::new(None),
            fetch_error: Mutex::new(None),
            fetch_ok_at: Mutex::new(None),
            fetch_round: AtomicU32::new(0),
            fetch_gate: Mutex::new(()),
            fetch_next_allowed_at: Mutex::new(Instant::now()),
            fetch_model_next_allowed_at: Mutex::new(HashMap::new()),
            fetch_burst_misses: Mutex::new(HashMap::new()),
            fetch_generation: AtomicU64::new(0),
            fetch_change_notify: Notify::new(),
            fetch_transition: Mutex::new(()),
            turn_state: Mutex::new(turn_state),
            http: Mutex::new(http),
            degraded: AtomicBool::new(false),
            degraded_at: Mutex::new(None),
            degrade_notify: Notify::new(),
            warp_wake: Notify::new(),
            model_notify: Notify::new(),
            seeds_registered: AtomicBool::new(false),
            #[cfg(test)]
            oauth_token_url: std::sync::Mutex::new(None),
        })
    }

    #[cfg(test)]
    pub(crate) fn set_oauth_token_url(&self, url: impl Into<String>) {
        *self.oauth_token_url.lock().expect("oauth url") = Some(url.into());
    }

    fn oauth_refresh_url(&self) -> Option<String> {
        #[cfg(test)]
        {
            return self.oauth_token_url.lock().expect("oauth url").clone();
        }
        #[cfg(not(test))]
        Some(login::oauth_token_endpoint().to_string())
    }

    async fn refresh_credentials_for_fetch(
        &self,
        settings: &Settings,
        model: &str,
    ) -> std::result::Result<login::ChatGptCredentials, FetchOnceError> {
        let home = Path::new(&settings.codex_home);
        let creds = match login::chatgpt_credentials(home) {
            Ok(creds) => creds,
            Err(err) => {
                let message = format!("{err:#}");
                self.defer_fetch_failure(model, FetchRetryClass::Auth)
                    .await;
                *self.fetch_error.lock().await = Some(message.clone());
                return Err(FetchOnceError::new(message, FetchRetryClass::Auth));
            }
        };
        // 写死禁用：打票前不再用 RT 换新 AT/RT，直接用当前登录文件里的凭证。
        const REFRESH_CREDENTIALS_BEFORE_FETCH: bool = false;
        if !REFRESH_CREDENTIALS_BEFORE_FETCH || !creds.refreshable {
            return Ok(creds);
        }
        let Some(token_url) = self.oauth_refresh_url() else {
            return Ok(creds);
        };
        let client = match login::token_import_http_client() {
            Ok(client) => client,
            Err(err) => {
                let message = format!("[{model}] 打票前刷新登录凭证失败: {err:#}");
                self.defer_fetch_failure(model, FetchRetryClass::Auth)
                    .await;
                *self.fetch_error.lock().await = Some(message.clone());
                return Err(FetchOnceError::new(message, FetchRetryClass::Auth));
            }
        };
        match login::refresh_session_credentials(home, &client, &token_url).await {
            Ok(creds) => Ok(creds),
            Err(err) => {
                let message = format!("[{model}] 打票前刷新登录凭证失败: {err:#}");
                self.defer_fetch_failure(model, FetchRetryClass::Auth)
                    .await;
                *self.fetch_error.lock().await = Some(message.clone());
                Err(FetchOnceError::new(message, FetchRetryClass::Auth))
            }
        }
    }

    async fn sync_request_identity(
        &self,
        home: &Path,
    ) -> Option<(login::ChatGptCredentials, bool)> {
        let _transition = self.fetch_transition.lock().await;
        let mut identity = login::request_credentials(home).ok()?;
        let needs_change = !self
            .turn_state
            .lock()
            .await
            .is_bound_to_account(&identity.0.account_id);
        if !needs_change {
            return Some(identity);
        }

        // Interrupt old cooldown waiters before acquiring the gate they hold.
        // The guard also restores an even generation if this request is cancelled.
        let generation_change = IdentityGenerationChange::new(self);
        // Wait for an in-flight probe, then read credentials again while the
        // transition lock prevents another account/config switch. The same
        // snapshot is returned for request authentication and ticket lookup.
        let _gate = self.fetch_gate.lock().await;
        identity = login::request_credentials(home).ok()?;
        let needs_change = !self
            .turn_state
            .lock()
            .await
            .is_bound_to_account(&identity.0.account_id);
        if needs_change {
            self.turn_state
                .lock()
                .await
                .bind_account(&identity.0.account_id);
            self.seeds_registered.store(false, Ordering::Relaxed);
            *self.fetch_error.lock().await = None;
            *self.fetch_ok_at.lock().await = None;
            self.reset_fetch_schedule().await;
            self.reset_fetch_burst().await;
            self.fetch_change_notify.notify_waiters();
            self.model_notify.notify_one();
        }
        drop(generation_change);
        Some(identity)
    }

    async fn sync_logged_in_account(&self) {
        let home = self.settings.lock().await.codex_home.clone();
        let _ = self.sync_request_identity(Path::new(&home)).await;
    }

    /// 业务响应只作观测。292 且模型一致时不续票；312 或完整响应模型不符时，
    /// 仅作废这次注入、且仍在池里的那张票。后到的旧响应不能删掉更新的票。
    async fn observe_business_response(
        &self,
        model: &str,
        injected_token: Option<&str>,
        returned_is_degraded: bool,
        upstream_model: Option<&str>,
        completed: bool,
    ) {
        let model_mismatch = completed
            && upstream_model.is_some_and(|actual| !actual.is_empty() && actual != model);
        if !returned_is_degraded && !model_mismatch {
            return;
        }
        let Some(injected_token) = injected_token.map(str::trim).filter(|token| !token.is_empty()) else {
            return;
        };
        let cleared = {
            let mut store = self.turn_state.lock().await;
            if store.peek_for_model(model).as_deref() != Some(injected_token) {
                false
            } else {
                store.invalidate_model(model);
                true
            }
        };
        if !cleared {
            return;
        }
        debug_log(&format!(
            "[degraded] [{model}] 业务响应不合格，作废当前凭据包并等待重采"
        ));
        self.degraded.store(true, Ordering::Relaxed);
        *self.degraded_at.lock().await = Some(
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        );
        self.degrade_notify.notify_one();
    }

    pub async fn status(&self) -> Status {
        self.sync_logged_in_account().await;
        let settings = self.settings.lock().await.clone();
        let logs = self
            .logs
            .lock()
            .await
            .iter()
            .map(LogEntry::snapshot)
            .collect();
        let login_status = login::login_status(Path::new(&settings.codex_home));
        let account = login_status.account_id;
        let account_traffic = self.traffic.view(account.as_deref(), Instant::now());
        let attached = is_attached(
            Path::new(&settings.codex_home),
            &format!("http://{}", settings.proxy_listen),
        );
        Status {
            proxy_listen: settings.proxy_listen,
            upstream: settings.upstream,
            codex_home: settings.codex_home,
            proxy_ok: self.proxy_ok.load(Ordering::Relaxed),
            attached,
            proxy_error: self.proxy_error.lock().await.clone(),
            attach_error: None,
            outbound_proxy: settings.outbound_proxy,
            upstream_proxy: settings.upstream_proxy,
            outbound_mode: settings.outbound_mode,
            warp_http2: settings.warp_http2,
            warp: self.warp.status(),
            mihomo_subscription: settings.mihomo_subscription.clone(),
            mihomo_node: settings.mihomo_node.clone(),
            mihomo: self.mihomo.status(),
            fetch_error: self.fetch_error.lock().await.clone(),
            fetch_ok_at: self.fetch_ok_at.lock().await.clone(),
            turn_state: self.turn_state.lock().await.view(),
            degraded: self.degraded.load(Ordering::Relaxed),
            degraded_at: self.degraded_at.lock().await.clone(),
            logs,
            account_traffic,
            current_account_id: account,
            current_account_email: login_status.email,
            state_miss_policy: settings.state_miss_policy,
            token_reuse_policy: settings.token_reuse_policy,
            state_fetch_model: settings.state_fetch_model,
            network_route_policy: settings.network_route_policy,
            forced_model: settings.forced_model,
            configured_models: settings.models,
            token_fetch_paused: settings.token_fetch_paused,
            token_max_age_mins: settings.token_max_age_mins,
            token_prefetch_age_mins: settings.token_prefetch_age_mins,
            diag_log_path: diag::path().display().to_string(),
        }
    }

    pub async fn refresh_turn_state(&self) -> Result<Status> {
        self.sync_logged_in_account().await;
        // A click is a one-shot probe: drop cooldown and ignore the pause flag
        // used by the background loop. fetch_once already skips only_if_needed.
        self.reset_fetch_schedule().await;
        *self.fetch_error.lock().await = Some(TOKEN_MANUAL_REFRESH_MESSAGE.into());
        self.fetch_change_notify.notify_waiters();
        let settings = self.settings.lock().await.clone();
        let mut models = settings.models.clone();
        if models.is_empty() {
            models = self.turn_state.lock().await.all_active_models();
        }
        if models.is_empty() {
            let model = settings
                .forced_model()
                .map(str::to_string)
                .unwrap_or_else(|| fetch::preferred_model(Path::new(&settings.codex_home)));
            self.turn_state.lock().await.register_model(&model);
            models.push(model);
        } else if let Some(model) = settings.forced_model() {
            if !models.iter().any(|item| item == model) {
                self.turn_state.lock().await.register_model(model);
                models.insert(0, model.to_string());
            }
        }
        let pinned_donor = {
            let store = self.turn_state.lock().await;
            settings
                .shared_state_donor()
                .filter(|donor| store.shares_292_for(donor))
                .map(str::to_string)
        };
        if let Some(donor) = pinned_donor.as_deref() {
            self.turn_state.lock().await.register_model(donor);
            if !models.iter().any(|item| item == donor) {
                models.insert(0, donor.to_string());
            }
        }
        let mut errors = Vec::new();
        let mut shared_refreshed = false;
        for model in &models {
            let shared = self.turn_state.lock().await.shares_292_for(model);
            if shared
                && (pinned_donor.as_deref().is_some_and(|donor| donor != model) || shared_refreshed)
            {
                continue;
            }
            if let Err(e) = self.fetch_once(model).await {
                eprintln!("[refresh] 模型 {} 获取失败: {e}", model);
                let can_try_other_model =
                    matches!(e.retry, FetchRetryClass::Forbidden | FetchRetryClass::Deferred);
                if !can_try_other_model {
                    return Err(e.into());
                }
                errors.push((model.clone(), e));
            } else if shared {
                shared_refreshed = true;
            }
        }
        // A forbidden donor does not fail the refresh if another model supplied
        // the shared ticket. Independent model failures still remain errors.
        for (model, err) in errors {
            let store = self.turn_state.lock().await;
            if !store.shares_292_for(&model) || store.needs_refresh(&model) {
                return Err(err.into());
            }
        }
        {
            let mut error = self.fetch_error.lock().await;
            if error.as_deref() == Some(TOKEN_MANUAL_REFRESH_MESSAGE) {
                *error = None;
            }
        }
        self.fetch_change_notify.notify_waiters();
        Ok(self.status().await)
    }

    /// 用户切换绑定的 token 长度（传 None 恢复账号自动识别的 292/332）
    pub async fn set_bound_token_len(&self, len: Option<usize>) -> Status {
        let _transition = self.fetch_transition.lock().await;
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.fetch_change_notify.notify_waiters();
        let _gate = self.fetch_gate.lock().await;
        {
            let mut store = self.turn_state.lock().await;
            store.set_bound_len(len);
        }
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.reset_fetch_schedule().await;
        self.fetch_change_notify.notify_waiters();
        drop(_gate);
        drop(_transition);
        self.degrade_notify.notify_one();
        self.status().await
    }

    pub async fn set_model_bound_token_len(&self, model: &str, len: Option<usize>) -> Status {
        let _transition = self.fetch_transition.lock().await;
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.fetch_change_notify.notify_waiters();
        let _gate = self.fetch_gate.lock().await;
        {
            let mut store = self.turn_state.lock().await;
            store.set_model_bound_len(model, len);
        }
        self.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.reset_fetch_schedule().await;
        self.fetch_change_notify.notify_waiters();
        drop(_gate);
        drop(_transition);
        self.degrade_notify.notify_one();
        self.status().await
    }

    fn fetch_settings(&self, settings: &Settings) -> Result<Settings> {
        let mut effective = settings.clone();
        match effective.outbound_mode {
            OutboundMode::Warp => effective.outbound_proxy = self.warp.proxy_url()?,
            OutboundMode::Mihomo => effective.outbound_proxy = self.mihomo.proxy_url()?,
            OutboundMode::Manual => {}
        }
        if effective.outbound_proxy.trim().is_empty() {
            anyhow::bail!("尚未配置出站代理");
        }
        Ok(effective)
    }

    async fn refresh_business_http(&self) -> Result<()> {
        let settings = self.settings.lock().await.clone();
        let proxy = resolved_business_proxy(&settings, &self.warp, &self.mihomo);
        let session = self.turn_state.lock().await.bound_proxy_session();
        *self.http.lock().await = business_http_client(&proxy, session.as_deref())?;
        Ok(())
    }

    async fn wait_for_fetch_slot(&self, generation: u64) -> bool {
        loop {
            let notified = self.fetch_change_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.fetch_generation.load(Ordering::SeqCst) != generation {
                return false;
            }
            let wait = {
                let next = *self.fetch_next_allowed_at.lock().await;
                next.saturating_duration_since(Instant::now())
            };
            if wait.is_zero() {
                return true;
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {},
                _ = &mut notified => {},
            }
        }
    }

    async fn defer_next_fetch(&self, delay: Duration) {
        *self.fetch_next_allowed_at.lock().await = Instant::now() + delay;
    }

    async fn reset_fetch_schedule(&self) {
        *self.fetch_next_allowed_at.lock().await = Instant::now();
        self.fetch_model_next_allowed_at.lock().await.clear();
    }

    async fn reset_fetch_burst(&self) {
        self.fetch_burst_misses.lock().await.clear();
    }

    async fn current_burst(&self, key: &str) -> usize {
        let misses = self
            .fetch_burst_misses
            .lock()
            .await
            .get(key)
            .copied()
            .unwrap_or(0);
        fetch::fetch_burst_concurrency(misses)
    }

    async fn note_burst_success(&self, key: &str) {
        self.fetch_burst_misses.lock().await.remove(key);
    }

    async fn note_burst_miss(&self, key: &str) {
        let mut misses = self.fetch_burst_misses.lock().await;
        let next = misses
            .get(key)
            .copied()
            .unwrap_or(0)
            .saturating_add(1)
            .min(fetch::MAX_FETCH_BURST.ilog2());
        misses.insert(key.to_string(), next);
    }

    async fn defer_fetch_failure(&self, model: &str, class: FetchRetryClass) {
        if matches!(class, FetchRetryClass::Stale | FetchRetryClass::Deferred) {
            return;
        }
        if class == FetchRetryClass::Forbidden {
            self.defer_next_fetch(fetch::RETRY_INTERVAL).await;
            self.fetch_model_next_allowed_at
                .lock()
                .await
                .insert(model.to_string(), Instant::now() + fetch_retry_delay(class));
        } else {
            self.defer_next_fetch(fetch_retry_delay(class)).await;
        }
    }

    async fn clear_model_fetch_delay(&self, model: &str) {
        self.fetch_model_next_allowed_at.lock().await.remove(model);
    }

    async fn model_fetch_wait(&self, model: &str) -> Duration {
        let now = Instant::now();
        let mut deadlines = self.fetch_model_next_allowed_at.lock().await;
        match deadlines.get(model).copied() {
            Some(deadline) if deadline > now => deadline.duration_since(now),
            Some(_) => {
                deadlines.remove(model);
                Duration::ZERO
            }
            None => Duration::ZERO,
        }
    }

    async fn eligible_fetch_models(&self, models: &[String]) -> (Vec<String>, Duration) {
        let now = Instant::now();
        let global_wait = self
            .fetch_next_allowed_at
            .lock()
            .await
            .saturating_duration_since(now);
        let mut deadlines = self.fetch_model_next_allowed_at.lock().await;
        deadlines.retain(|_, deadline| *deadline > now);

        let eligible: Vec<String> = models
            .iter()
            .filter(|model| !deadlines.contains_key(model.as_str()))
            .cloned()
            .collect();
        if !eligible.is_empty() {
            return (eligible, global_wait);
        }

        let model_wait = models
            .iter()
            .filter_map(|model| deadlines.get(model.as_str()))
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
            .unwrap_or(fetch::CHECK_INTERVAL);
        // Re-evaluate all active models periodically while every currently
        // stale model is cooling. A different model may enter its prefetch
        // window before this model's (notably 403) cooldown expires.
        (eligible, global_wait.max(model_wait.min(fetch::CHECK_INTERVAL)))
    }

    async fn fetch_once(&self, model: &str) -> std::result::Result<String, FetchOnceError> {
        self.fetch_once_inner(model, false).await
    }

    async fn fetch_once_inner(&self, model: &str, only_if_needed: bool) -> std::result::Result<String, FetchOnceError> {
        let _gate = self.fetch_gate.lock().await;
        let generation = self.fetch_generation.load(Ordering::SeqCst);
        if generation % 2 == 1 {
            return Err(FetchOnceError::new(
                format!("[{model}] 配置切换中，暂不获取票据"),
                FetchRetryClass::Normal,
            ));
        }
        if only_if_needed && self.settings.lock().await.token_fetch_paused {
            return Err(FetchOnceError::new(
                TOKEN_FETCH_PAUSED_MESSAGE,
                FetchRetryClass::Deferred,
            ));
        }
        let model_wait = self.model_fetch_wait(model).await;
        if !model_wait.is_zero() {
            return Err(FetchOnceError::new(
                format!(
                    "[{model}] 票据获取仍在独立冷却中（剩余约 {} 秒）",
                    model_wait.as_secs().saturating_add(1)
                ),
                FetchRetryClass::Deferred,
            ));
        }
        let saved = self.settings.lock().await.clone();
        let settings = match self.fetch_settings(&saved) {
            Ok(settings) => settings,
            Err(err) => {
                let message = err.to_string();
                self.defer_fetch_failure(model, FetchRetryClass::Backoff)
                    .await;
                *self.fetch_error.lock().await = Some(message.clone());
                return Err(FetchOnceError::new(message, FetchRetryClass::Backoff));
            }
        };
        if !has_chatgpt_login(Path::new(&settings.codex_home)) {
            let message = "尚未登录 ChatGPT".to_string();
            self.defer_fetch_failure(model, FetchRetryClass::Auth)
                .await;
            *self.fetch_error.lock().await = Some(message.clone());
            return Err(FetchOnceError::new(message, FetchRetryClass::Auth));
        }
        let creds = match login::chatgpt_credentials(Path::new(&settings.codex_home)) {
            Ok(creds) => creds,
            Err(err) => {
                let message = format!("{err:#}");
                self.defer_fetch_failure(model, FetchRetryClass::Auth)
                    .await;
                *self.fetch_error.lock().await = Some(message.clone());
                return Err(FetchOnceError::new(message, FetchRetryClass::Auth));
            }
        };
        let (target_len, allow_auto_quality, account_matches) = {
            let store = self.turn_state.lock().await;
            (
                store.bound_len_for(model),
                store.allows_auto_quality_discovery(model),
                store.is_bound_to_account(&creds.account_id),
            )
        };
        if !account_matches {
            return Err(FetchOnceError::new(
                format!("[{model}] 登录账号与票据池绑定账号不一致"),
                FetchRetryClass::Stale,
            ));
        }

        if only_if_needed && fetch_account_is_current(&settings, &creds.account_id) {
            let store = self.turn_state.lock().await;
            if !store.needs_refresh(model) {
                if let Some(token) = store.peek_for_model(model) {
                    return Ok(token);
                }
            }
        }
        let creds = self.refresh_credentials_for_fetch(&settings, model).await?;
        if !self
            .turn_state
            .lock()
            .await
            .is_bound_to_account(&creds.account_id)
        {
            return Err(FetchOnceError::new(
                format!("[{model}] 登录账号与票据池绑定账号不一致"),
                FetchRetryClass::Stale,
            ));
        }
        if !self.wait_for_fetch_slot(generation).await {
            let message = format!("[{model}] 配置已变化，取消旧线路票据请求");
            return Err(FetchOnceError::new(message, FetchRetryClass::Stale));
        }
        if only_if_needed && self.settings.lock().await.token_fetch_paused {
            return Err(FetchOnceError::new(
                TOKEN_FETCH_PAUSED_MESSAGE,
                FetchRetryClass::Deferred,
            ));
        }
        if !fetch_account_is_current(&settings, &creds.account_id) {
            return Err(FetchOnceError::new(format!("[{model}] 登录账号已变化，取消旧账号票据请求"), FetchRetryClass::Stale));
        }
        if only_if_needed {
            let store = self.turn_state.lock().await;
            if !store.needs_refresh(model) {
                if let Some(token) = store.peek_for_model(model) {
                    return Ok(token);
                }
            }
        }
        let burst_key = {
            let store = self.turn_state.lock().await;
            burst_key_for(&store, model)
        };
        let concurrency = self.current_burst(&burst_key).await;
        debug_log(&format!(
            "[{model}] 本波 {concurrency} 并发探测，目标长度 {target_len}"
        ));
        let outcomes = join_all((0..concurrency).map(|_| {
            self.run_single_probe(
                model,
                &settings,
                &creds,
                target_len,
                allow_auto_quality,
                generation,
            )
        }))
        .await;
        if let Some(token) = outcomes.iter().find_map(|item| item.as_ref().ok()).cloned() {
            self.note_burst_success(&burst_key).await;
            self.defer_next_fetch(fetch::CHECK_INTERVAL).await;
            self.clear_model_fetch_delay(model).await;
            if let Err(err) = self.refresh_business_http().await {
                eprintln!("[{model}] 绑定出口 session 后刷新业务代理失败: {err:#}");
            }
            *self.fetch_error.lock().await = None;
            *self.fetch_ok_at.lock().await = Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            );
            return Ok(token);
        }
        let mut lens: HashMap<usize, u32> = HashMap::new();
        let mut first_err: Option<FetchOnceError> = None;
        let mut escalate = false;
        for outcome in outcomes {
            let Err(err) = outcome else { continue };
            escalate |= err.escalate;
            if let Some(len) = err.returned_len {
                *lens.entry(len).or_insert(0) += 1;
            }
            if first_err
                .as_ref()
                .is_none_or(|seen| retry_priority(err.retry) < retry_priority(seen.retry))
            {
                first_err = Some(err);
            }
        }
        if !lens.is_empty() {
            let distribution = lens
                .into_iter()
                .map(|(len, count)| turn_state::TokenLenCount { len, count })
                .collect();
            self.turn_state
                .lock()
                .await
                .record_distribution(model, distribution);
        }
        let mut err = first_err.unwrap_or_else(|| {
            FetchOnceError::new(format!("[{model}] 本波探测未返回结果"), FetchRetryClass::Normal)
        });
        if let Some(rest) = length_miss_rest(concurrency, err.retry, escalate) {
            self.fetch_burst_misses.lock().await.remove(&burst_key);
            self.defer_next_fetch(rest).await;
            err.message.push_str(&format!(
                "；已达最大并发 {} 仍未命中，静置 {} 秒后从 1 路重试",
                fetch::MAX_FETCH_BURST,
                rest.as_secs()
            ));
        } else {
            if escalate && err.retry != FetchRetryClass::Auth {
                self.note_burst_miss(&burst_key).await;
            }
            self.defer_fetch_failure(model, err.retry).await;
        }
        *self.fetch_error.lock().await = Some(err.message.clone());
        Err(err)
    }

    async fn run_single_probe(
        &self,
        model: &str,
        settings: &Settings,
        creds: &login::ChatGptCredentials,
        target_len: usize,
        allow_auto_quality: bool,
        generation: u64,
    ) -> std::result::Result<String, FetchOnceError> {
        if self.fetch_generation.load(Ordering::SeqCst) != generation {
            return Err(FetchOnceError::new(
                format!("[{model}] 配置已变化，取消旧线路票据请求"),
                FetchRetryClass::Stale,
            ));
        }
        if !fetch_account_is_current(settings, &creds.account_id) {
            return Err(FetchOnceError::new(
                format!("[{model}] 登录账号已变化，取消旧账号票据请求"),
                FetchRetryClass::Stale,
            ));
        }
        let (probe_proxy, probe_session) = fetch::resolve_probe_proxy(&settings.outbound_proxy);
        let mut fetch_settings = settings.clone();
        fetch_settings.outbound_proxy = probe_proxy;
        let client = match fetch::http_client(&fetch_settings.outbound_proxy) {
            Ok(client) => client,
            Err(err) => {
                let message = format!("{err:#}");
                return Err(FetchOnceError::probing(message, FetchRetryClass::Backoff, true));
            }
        };
        let started = Instant::now();
        let mut details = NetworkLogDetails::default();
        let request_cookies = self.turn_state.lock().await.bound_routing_cookies();
        let result = fetch::fetch_turn_state_with_cookies(
            &client,
            &fetch_settings,
            creds,
            model,
            target_len,
            allow_auto_quality,
            &request_cookies,
            &mut details,
        )
        .await;
        details.proxy_session = probe_session.clone();

        if !fetch_account_is_current(settings, &creds.account_id) {
            details.turn_state_action = "discarded_stale_account".into();
            self.record_fetch(started, details).await;
            return Err(FetchOnceError::new(
                format!("[{model}] 登录账号已变化，丢弃旧账号票据结果"),
                FetchRetryClass::Stale,
            ));
        }
        match result {
            Ok(fetched) => {
                if !settings.same_network() {
                    let business = resolved_business_proxy(settings, &self.warp, &self.mihomo);
                    let verify_client = match upstream_http_client(&business) {
                        Ok(client) => client,
                        Err(err) => {
                            details.turn_state_action = "rejected_reverify".into();
                            self.record_fetch(started, details).await;
                            return Err(FetchOnceError::probing(
                                format!("[{model}] 业务出口复验失败: {err:#}"),
                                FetchRetryClass::Normal,
                                true,
                            ));
                        }
                    };
                    let mut verify_settings = fetch_settings.clone();
                    verify_settings.outbound_proxy = business;
                    let mut verify_details = NetworkLogDetails::default();
                    if let Err(err) = fetch::validate_carried_ticket(
                        &verify_client,
                        &verify_settings,
                        creds,
                        model,
                        &fetched.token,
                        &fetched.routing_cookies,
                        &mut verify_details,
                    )
                    .await
                    {
                        details.turn_state_action = "rejected_reverify".into();
                        self.record_fetch(started, details).await;
                        return Err(FetchOnceError::probing(
                            format!("[{model}] 业务出口复验未通过: {err:#}"),
                            FetchRetryClass::Normal,
                            true,
                        ));
                    }
                }
                let token = fetched.token;
                if self.fetch_generation.load(Ordering::SeqCst) != generation {
                    details.turn_state_action = "discarded_stale_config".into();
                    self.record_fetch(started, details).await;
                    return Err(FetchOnceError::new(
                        format!("[{model}] 配置已变化，丢弃旧线路返回的票据"),
                        FetchRetryClass::Stale,
                    ));
                }
                let ready = {
                    let mut store = self.turn_state.lock().await;
                    if self.fetch_generation.load(Ordering::SeqCst) != generation
                        || !store.is_bound_to_account(&creds.account_id)
                    {
                        None
                    } else {
                        Some(capture_fetched_ticket(
                            &mut store,
                            model,
                            &token,
                            probe_session.as_deref(),
                            fetched.previous_response_id.as_deref(),
                            &fetched.routing_cookies,
                        ))
                    }
                };
                let Some(ready) = ready else {
                    details.turn_state_action = "discarded_stale_config".into();
                    self.record_fetch(started, details).await;
                    return Err(FetchOnceError::new(
                        format!("[{model}] 配置已变化，丢弃旧线路返回的票据"),
                        FetchRetryClass::Stale,
                    ));
                };
                if !ready {
                    details.turn_state_action = "pooled_unmatched".into();
                    self.record_fetch(started, details).await;
                    return Err(FetchOnceError::probing(
                        format!(
                            "[{model}] 采到 {} 字节票据，但未匹配请求开始时的目标长度 {target_len}",
                            token.len()
                        ),
                        FetchRetryClass::Normal,
                        true,
                    ));
                }
                details.turn_state_action = "captured".into();
                details.token_fp = Some(diag::token_fp(&token));
                details.cookie_names = chatgpt_cookies::cookie_names(&fetched.routing_cookies);
                self.record_fetch(started, details).await;
                Ok(token)
            }
            Err(err) => {
                if self.fetch_generation.load(Ordering::SeqCst) != generation {
                    details.turn_state_action = "discarded_stale_config".into();
                    self.record_fetch(started, details).await;
                    return Err(FetchOnceError::new(
                        format!("[{model}] 配置已变化，忽略旧线路请求错误"),
                        FetchRetryClass::Stale,
                    ));
                }
                let retry_class = classify_fetch_failure(&details);
                let escalate = fetch_failure_escalates(&details);
                let returned_len = details.returned_turn_state_len;
                self.record_fetch(started, details).await;
                let message = format!("{err:#}");
                eprintln!("[{model}] turn-state fetch failed: {message}");
                Err(FetchOnceError::probing(message, retry_class, escalate).with_len(returned_len))
            }
        }
    }

    async fn refresh_if_needed(&self) -> Duration {
        let saved = self.settings.lock().await.clone();
        if saved.token_fetch_paused {
            *self.fetch_error.lock().await = Some(TOKEN_FETCH_PAUSED_MESSAGE.into());
            return fetch::CHECK_INTERVAL;
        }
        let settings = match self.fetch_settings(&saved) {
            Ok(settings) => settings,
            Err(err) => {
                *self.fetch_error.lock().await = Some(err.to_string());
                return Duration::from_secs(30);
            }
        };
        if !has_chatgpt_login(Path::new(&settings.codex_home)) {
            *self.fetch_error.lock().await = Some("尚未登录 ChatGPT".into());
            return Duration::from_secs(30);
        }
        self.sync_logged_in_account().await;

        // 首次运行：注册 settings.models 中的种子模型
        if !self.seeds_registered.swap(true, Ordering::Relaxed) {
            let mut store = self.turn_state.lock().await;
            for model in &settings.models {
                if !model.is_empty() && store.register_model(model) {
                    eprintln!("[seed] 从设置注册种子模型: {}", model);
                }
            }
            if let Some(model) = settings.forced_model() {
                if store.register_model(model) {
                    eprintln!("[seed] 从强制绑定注册模型: {}", model);
                }
            }
            if let Some(model) = settings.shared_state_donor() {
                if store.register_model(model) {
                    eprintln!("[seed] 跨模型复用指定取票模型: {}", model);
                }
            }
        }

        // 312 降智信号 → 清池（所有模型的 token，但保留追踪）
        if self.degraded.swap(false, Ordering::Relaxed) {
            eprintln!("312 降智 / 服务端拒绝信号，清池重打 292（所有模型）");
            self.turn_state.lock().await.invalidate_all();
            *self.degraded_at.lock().await = None;
        }

        // 获取所有活跃模型（最近 60 分钟内有请求的），检查哪些需要刷新。
        // 指定了取票模型时，共享 292 只向该模型索取。
        let donor = settings.shared_state_donor().map(str::to_string);
        let models_needing_refresh: Vec<String> = {
            let store = self.turn_state.lock().await;
            let models: Vec<String> = store
                .all_active_models()
                .into_iter()
                .filter(|m| store.needs_refresh(m))
                .collect();
            match donor.as_deref() {
                Some(donor) if store.shares_292_for(donor) => pin_shared_donor(&store, &models, donor),
                _ => models,
            }
        };

        if models_needing_refresh.is_empty() {
            return fetch::CHECK_INTERVAL;
        }

        let (eligible_models, wait) = self.eligible_fetch_models(&models_needing_refresh).await;
        if eligible_models.is_empty() {
            return wait;
        }

        let round = self.fetch_round.fetch_add(1, Ordering::Relaxed) + 1;
        let selected = {
            let store = self.turn_state.lock().await;
            model_for_reuse_round(&store, &eligible_models, round)
                .expect("eligible_models is not empty").to_string()
        };
        let model = selected.as_str();
        let bound_len = self.turn_state.lock().await.bound_len_for(model);
        let burst = {
            let store = self.turn_state.lock().await;
            let key = burst_key_for(&store, model);
            drop(store);
            self.current_burst(&key).await
        };
        *self.fetch_error.lock().await = Some(if burst > 1 {
            format!("正在以 {burst} 并发获取 {model} 的 {bound_len} Token（第 {round} 轮）…")
        } else {
            format!("正在获取 {model} 的 {bound_len} Token（第 {round} 轮）…")
        });
        debug_log(&format!(
            "[{}] 需要新 token，第 {} 轮 {} 并发获取，目标长度 {}",
            model, round, burst, bound_len
        ));

        match self.fetch_once_inner(model, true).await {
            Ok(token) => {
                debug_log(&format!("✅ [{}] 命中 {} 字节票据", model, token.len()));
                let remaining = {
                    let store = self.turn_state.lock().await;
                    store
                        .all_active_models()
                        .into_iter()
                        .any(|active| store.needs_refresh(&active))
                };
                if remaining {
                    fetch::RETRY_INTERVAL
                } else {
                    fetch::CHECK_INTERVAL
                }
            }
            Err(err) => {
                let message = format!("{err:#}");
                eprintln!("[{}] 本波未命中: {message}", model);
                if err.retry != FetchRetryClass::Stale {
                    *self.fetch_error.lock().await = Some(message.clone());
                }
                let (_, wait) = self.eligible_fetch_models(&models_needing_refresh).await;
                wait
            }
        }
    }

    async fn record(
        &self,
        method: &str,
        path: &str,
        status: u16,
        started: Instant,
        details: NetworkLogDetails,
    ) {
        if let Some(req) = &details.diag {
            diag::emit(
                "finish",
                Some(req),
                json!({
                    "status": status,
                    "error": details.error_kind,
                    "headerMs": details.response_header_ms,
                    "peer": details.peer_addr,
                    "http": details.http_version,
                }),
            );
        } else if details.flow == "token_fetch" {
            diag::emit(
                "token_fetch",
                None,
                json!({
                    "action": details.turn_state_action,
                    "model": details.model,
                    "session": details.proxy_session,
                    "status": details.response_status.unwrap_or(status),
                    "returnedStateLen": details.returned_turn_state_len,
                    "routeKind": details.route_kind,
                    "peer": details.peer_addr,
                    "http": details.http_version,
                    "headerMs": details.response_header_ms,
                    "error": details.error_kind,
                    "tokenFp": details.token_fp,
                    "cookies": details.cookie_names,
                }),
            );
        }
        let entry = LogEntry::new(method, path, status, started, details);
        let mut logs = self.logs.lock().await;
        logs::push(&mut logs, entry);
    }

    async fn record_fetch(&self, started: Instant, details: NetworkLogDetails) {
        let status = details.response_status.unwrap_or(502);
        self.record("POST", "/responses", status, started, details)
            .await;
    }
}

#[derive(Clone)]
pub struct ProxyHandle {
    app: Arc<App>,
    stop: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
    fetch_stop: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    fetch_task: Arc<Mutex<Option<JoinHandle<()>>>>,
    settings_change: Arc<Mutex<()>>,
    managed_routes: Arc<std::sync::Mutex<Option<attach::ManagedRoutes>>>,
    attach_error: Arc<std::sync::Mutex<Option<String>>>,
}

impl ProxyHandle {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            stop: Arc::new(Mutex::new(None)),
            task: Arc::new(Mutex::new(None)),
            fetch_stop: Arc::new(Mutex::new(None)),
            fetch_task: Arc::new(Mutex::new(None)),
            settings_change: Arc::new(Mutex::new(())),
            managed_routes: Arc::new(std::sync::Mutex::new(None)),
            attach_error: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    pub fn app(&self) -> Arc<App> {
        self.app.clone()
    }

    pub fn enable_auto_attach(&self) {
        *self.managed_routes.lock().expect("managed routes") = Some(attach::ManagedRoutes::new(attach::backup_path()));
    }

    pub fn restore_managed_routes(&self) -> Result<()> {
        if let Some(routes) = self.managed_routes.lock().expect("managed routes").as_mut() {
            routes.shutdown()?;
        }
        Ok(())
    }

    fn sync_routes_to(&self, settings: &Settings) -> Result<()> {
        let result = match self.managed_routes.lock().expect("managed routes").as_mut() {
            Some(routes) => routes.sync(settings, self.app.proxy_ok.load(Ordering::Relaxed)),
            None => Ok(()),
        };
        *self.attach_error.lock().expect("attach error") = result.as_ref().err().map(|err| format!("自动接入失败：{err:#}"));
        result
    }

    pub async fn managed_status(&self) -> Status {
        let mut status = self.app.status().await;
        status.attach_error = self.attach_error.lock().expect("attach error").clone();
        status
    }

    pub fn core(&self) -> &App {
        &self.app
    }

    pub async fn run_attachment_supervisor(&self) {
        loop {
            {
                let _change = self.settings_change.lock().await;
                let settings = self.app.settings.lock().await.clone();
                let _ = self.sync_routes_to(&settings);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    pub async fn start_managed(&self) -> Result<()> {
        let _change = self.settings_change.lock().await;
        self.start().await
    }

    pub async fn start(&self) -> Result<()> {
        self.stop().await;
        let listen = self.app.settings.lock().await.proxy_listen.clone();
        let addr: SocketAddr = listen.parse().context("proxy_listen")?;
        let listener = match bind_listen(addr).await {
            Ok(listener) => listener,
            Err(err) => {
                self.app.proxy_ok.store(false, Ordering::Relaxed);
                let in_use = err.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::AddrInUse)
                }) || format!("{err:#}").contains("Address already in use");
                let message = if in_use {
                    format!("{addr} 已被占用，无法启动代理。请先关掉旧的 Codex State Kit 再试。")
                } else {
                    format!("无法绑定 {addr}: {err:#}")
                };
                *self.app.proxy_error.lock().await = Some(message);
                self.start_fetch_loop().await;
                return Err(err);
            }
        };
        *self.app.proxy_error.lock().await = None;
        self.restore_leftover().await;
        let (tx, rx) = oneshot::channel();
        *self.stop.lock().await = Some(tx);
        let app = self.app.clone();
        app.proxy_ok.store(true, Ordering::Relaxed);
        println!("proxy  http://{addr}  (point Codex openai_base_url here)");
        let task_app = app.clone();
        let handle = tokio::spawn(async move {
            let router = axum::Router::new()
                .fallback(proxy)
                .with_state(task_app.clone());
            let result = axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = rx.await;
                })
                .await;
            task_app.proxy_ok.store(false, Ordering::Relaxed);
            if let Err(err) = result {
                eprintln!("proxy stopped: {err}");
            }
        });
        *self.task.lock().await = Some(handle);
        let settings = self.app.settings.lock().await.clone();
        let _ = self.sync_routes_to(&settings);
        self.start_fetch_loop().await;
        Ok(())
    }

    async fn start_fetch_loop(&self) {
        self.stop_fetch_loop().await;
        let (tx, mut rx) = oneshot::channel();
        *self.fetch_stop.lock().await = Some(tx);
        let app = self.app.clone();
        let handle = tokio::spawn(async move {
            loop {
                let wait = app.refresh_if_needed().await;
                tokio::select! {
                    _ = &mut rx => break,
                    _ = tokio::time::sleep(wait) => {}
                    _ = app.degrade_notify.notified() => {
                        eprintln!("312 信号唤醒 fetch 循环，立即续期");
                    }
                    _ = app.model_notify.notified() => {
                        eprintln!("新模型发现，唤醒 fetch 循环");
                    }
                }
            }
        });
        *self.fetch_task.lock().await = Some(handle);
    }

    async fn stop_fetch_loop(&self) {
        if let Some(tx) = self.fetch_stop.lock().await.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.fetch_task.lock().await.take() {
            // Cancel the old route's in-flight fetches before a mode switch.
            handle.abort();
            let _ = handle.await;
        }
    }

    async fn restore_leftover(&self) {
        if self.app.leftover_restored.swap(true, Ordering::SeqCst) {
            return;
        }
        let home = self.app.settings.lock().await.codex_home.clone();
        match crate::attach::restore_codex_config(Path::new(&home)) {
            Ok(msg) if msg != "nothing to restore" => {
                println!("restored leftover Codex config: {msg}");
            }
            Err(err) => eprintln!("failed to restore leftover Codex config: {err:#}"),
            _ => {}
        }
    }

    pub async fn stop(&self) {
        self.stop_fetch_loop().await;
        if let Some(tx) = self.stop.lock().await.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.task.lock().await.take() {
            let _ = handle.await;
        }
        self.app.proxy_ok.store(false, Ordering::Relaxed);
    }

    pub async fn apply_settings(&self, patch: SettingsPatch) -> Result<Status> {
        let next = patch.into_settings()?;
        let _change = if next.outbound_mode == OutboundMode::Manual {
            loop {
                self.app.warp.cancel_connect();
                tokio::select! {
                    guard = self.settings_change.lock() => break guard,
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {},
                }
            }
        } else {
            self.settings_change.lock().await
        };
        let old = self.app.settings.lock().await.clone();
        let next_business = resolved_business_proxy(&next, &self.app.warp, &self.app.mihomo);
        let next_http = if resolved_business_proxy(&old, &self.app.warp, &self.app.mihomo) != next_business {
            Some(business_http_client(&next_business, None)?)
        } else {
            None
        };
        if old.codex_home != next.codex_home {
            attach::validate_codex_home(Path::new(&next.codex_home))?;
        }
        self.sync_routes_to(&next)?;
        if let Err(err) = save_settings(&next) {
            self.sync_routes_to(&old)?;
            return Err(err);
        }
        let route_changed = old.outbound_proxy != next.outbound_proxy
            || old.outbound_mode != next.outbound_mode
            || old.warp_http2 != next.warp_http2
            || old.upstream != next.upstream
            || old.codex_home != next.codex_home
            || old.mihomo_subscription != next.mihomo_subscription
            || old.mihomo_node != next.mihomo_node;
        let reuse_changed = old.token_reuse_policy != next.token_reuse_policy;
        let fetch_changed = route_changed || reuse_changed;
        let mut fetch_transition_guard = None;
        let mut fetch_change_guard = None;
        if fetch_changed {
            fetch_transition_guard = Some(self.app.fetch_transition.lock().await);
            self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
            self.app.fetch_change_notify.notify_waiters();
            self.stop_fetch_loop().await;
            fetch_change_guard = Some(self.app.fetch_gate.lock().await);
            if route_changed {
                self.app.turn_state.lock().await.invalidate_all();
            }
            *self.app.fetch_error.lock().await = None;
            *self.app.fetch_ok_at.lock().await = None;
            self.app.degraded.store(false, Ordering::Relaxed);
            *self.app.degraded_at.lock().await = None;
            if route_changed && next.outbound_mode != OutboundMode::Warp {
                self.app.warp.stop().await;
            }
            if route_changed && next.outbound_mode != OutboundMode::Mihomo {
                self.app.mihomo.stop().await;
            }
        }
        {
            let mut settings = self.app.settings.lock().await;
            *settings = next.clone();
            let mut store = self.app.turn_state.lock().await;
            store.set_reuse_policy(next.token_reuse_policy);
            store.set_lifetime(turn_state::MAX_AGE_SECS, turn_state::PREFETCH_AGE_SECS);
            if let Some(http) = next_http {
                *self.app.http.lock().await = http;
            }
        }
        if fetch_changed {
            self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
            // A policy-only switch is still the same upstream route/account:
            // preserve physical request spacing and any auth/rate-limit cooldown.
            if route_changed {
                self.app.reset_fetch_schedule().await;
            }
            self.app.reset_fetch_burst().await;
            self.app.fetch_change_notify.notify_waiters();
            drop(fetch_change_guard.take());
            drop(fetch_transition_guard.take());
        }
        if old.proxy_listen != next.proxy_listen {
            if let Err(err) = self.start().await {
                {
                    let mut settings = self.app.settings.lock().await;
                    settings.proxy_listen = old.proxy_listen.clone();
                    let _ = save_settings(&settings);
                }
                let _ = self.start().await;
                return Err(err);
            }
            // Managed routes are already synchronized by start(), under their exit lock.
            if self.managed_routes.lock().expect("managed routes").is_none() {
                attach::update_attached_base_url(&next)?;
            }
        } else if fetch_changed {
            self.start_fetch_loop().await;
        }
        if old.token_fetch_paused != next.token_fetch_paused {
            self.app.fetch_change_notify.notify_waiters();
            self.app.model_notify.notify_one();
            if next.token_fetch_paused {
                *self.app.fetch_error.lock().await = Some(TOKEN_FETCH_PAUSED_MESSAGE.into());
            } else if self.app.fetch_error.lock().await.as_deref() == Some(TOKEN_FETCH_PAUSED_MESSAGE)
            {
                *self.app.fetch_error.lock().await = None;
            }
        }
        if old.token_max_age_mins != next.token_max_age_mins
            || old.token_prefetch_age_mins != next.token_prefetch_age_mins
        {
            self.app.fetch_change_notify.notify_waiters();
            self.app.model_notify.notify_one();
        }
        if old.forced_model != next.forced_model {
            if let Some(model) = next.forced_model() {
                self.app.turn_state.lock().await.register_model(model);
            }
            self.app.model_notify.notify_one();
        }
        if old.state_fetch_model != next.state_fetch_model {
            if let Some(model) = next.shared_state_donor() {
                self.app.turn_state.lock().await.register_model(model);
            }
            self.app.model_notify.notify_one();
        }
        if next.outbound_mode == OutboundMode::Mihomo
            && (route_changed || self.app.mihomo.status().phase != "connected")
        {
            if let Err(err) = self.app.mihomo.start(&next).await {
                eprintln!("mihomo: {err:#}");
            }
            let _ = self.app.refresh_business_http().await;
        }
        self.app.warp_wake.notify_one();
        Ok(self.managed_status().await)
    }

    pub async fn run_warp_supervisor(&self) {
        loop {
            let mode = self.app.settings.lock().await.outbound_mode;
            let mut wait = Duration::from_secs(20);
            if mode == OutboundMode::Warp {
                let phase = self.app.warp.status().phase;
                if matches!(phase.as_str(), "stopped" | "error") {
                    if let Err(err) = self.connect_warp(true).await {
                        eprintln!("embedded WARP: {err:#}");
                        wait = Duration::from_secs(60);
                    }
                } else {
                    self.app.warp.check_health().await;
                }
            } else if mode == OutboundMode::Mihomo {
                let phase = self.app.mihomo.status().phase;
                if matches!(phase.as_str(), "stopped" | "error") {
                    let settings = self.app.settings.lock().await.clone();
                    if let Err(err) = self.app.mihomo.start(&settings).await {
                        eprintln!("mihomo: {err:#}");
                        wait = Duration::from_secs(60);
                    } else {
                        let _ = self.app.refresh_business_http().await;
                    }
                }
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {},
                _ = self.app.warp_wake.notified() => {},
            }
        }
    }

    pub async fn probe_latency(
        &self,
        kind: &str,
        proxy: Option<String>,
    ) -> Result<crate::latency::LatencyReport> {
        let settings = self.app.settings.lock().await.clone();
        let target = crate::latency::probe_target(&settings.upstream)?;
        let samples = match kind {
            "warp" => {
                let result = match self.app.warp.proxy_url() {
                    Ok(endpoint) => crate::latency::probe_through_proxy(&endpoint, &target).await,
                    Err(err) => Err(err),
                };
                vec![crate::latency::sample_from_result("warp", result)]
            }
            "manual" => {
                let raw = proxy.filter(|value| !value.trim().is_empty()).unwrap_or(settings.outbound_proxy);
                vec![crate::latency::sample_from_result(
                    "manual",
                    crate::latency::probe_through_proxy(&raw, &target).await,
                )]
            }
            "mihomo" => self.app.mihomo.probe_delays(&target).await?,
            _ => anyhow::bail!("未知的检测对象"),
        };
        Ok(crate::latency::LatencyReport { target, samples })
    }

    pub async fn connect_warp(&self, accept_terms: bool) -> Result<Status> {
        let _change = self.settings_change.lock().await;
        let settings = self.app.settings.lock().await.clone();
        if settings.outbound_mode != OutboundMode::Warp {
            anyhow::bail!("请先选择内置 WARP 模式");
        }
        let fetch_transition = self.app.fetch_transition.lock().await;
        self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.app.fetch_change_notify.notify_waiters();
        self.stop_fetch_loop().await;
        let fetch_change = self.app.fetch_gate.lock().await;
        let result = self
            .app
            .warp
            .connect(accept_terms, settings.warp_http2)
            .await;
        self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.app.reset_fetch_schedule().await;
        self.app.fetch_change_notify.notify_waiters();
        *self.app.fetch_error.lock().await = None;
        *self.app.fetch_ok_at.lock().await = None;
        drop(fetch_change);
        drop(fetch_transition);
        self.start_fetch_loop().await;
        result?;
        self.app.refresh_business_http().await?;
        Ok(self.app.status().await)
    }

    pub async fn stop_warp(&self) -> Status {
        let _change = self.settings_change.lock().await;
        let fetch_transition = self.app.fetch_transition.lock().await;
        self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.app.fetch_change_notify.notify_waiters();
        self.stop_fetch_loop().await;
        let fetch_change = self.app.fetch_gate.lock().await;
        self.app.warp.stop().await;
        let _ = self.app.refresh_business_http().await;
        self.app.fetch_generation.fetch_add(1, Ordering::SeqCst);
        self.app.reset_fetch_schedule().await;
        self.app.fetch_change_notify.notify_waiters();
        *self.app.fetch_error.lock().await = None;
        *self.app.fetch_ok_at.lock().await = None;
        drop(fetch_change);
        drop(fetch_transition);
        self.start_fetch_loop().await;
        self.app.status().await
    }
}

async fn bind_listen(addr: SocketAddr) -> Result<TcpListener> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(err)
                if err.kind() == std::io::ErrorKind::AddrInUse && Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(err) => return Err(err).with_context(|| format!("bind {addr}")),
        }
    }
}

async fn proxy(State(app): State<Arc<App>>, req: Request<Body>) -> Response {
    if is_websocket(&req) {
        // WebSocket 升级需要 Cloudflare cookie（由 Codex 客户端维护），
        // 代理自建的连接没有 cookie 会被 Cloudflare 403 拒绝。
        // 返回 426 Upgrade Required —— 官方 Codex 客户端检测到此状态码后
        // 会自动永久切换到 HTTP SSE 流式传输（见 client.rs FallbackToHttp 逻辑）。
        eprintln!("[ws] 拒绝 WS 升级（无 Cloudflare cookie），返回 426 触发客户端回退到 HTTP SSE");
        return (StatusCode::UPGRADE_REQUIRED, "WebSocket not supported by proxy, use HTTP SSE").into_response();
    }
    proxy_http(app, req).await
}

fn is_websocket(req: &Request<Body>) -> bool {
    req.headers()
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false)
}

async fn proxy_http(app: Arc<App>, req: Request<Body>) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = logs::safe_text(req.uri().path(), 256);
    let mut details = NetworkLogDetails::default();
    let mut activity = None;
    match forward_http_tracked(&app, req, &mut details, &mut activity, started).await {
        Ok(resp) => {
            details.response_header_ms = Some(started.elapsed().as_millis());
            details.response_content_encoding = Some(logs::safe_content_encoding(
                resp.headers().get(header::CONTENT_ENCODING).and_then(|value| value.to_str().ok()),
            ));
            if method == http::Method::HEAD
                || matches!(resp.status().as_u16(), 204 | 304)
                || resp
                    .headers()
                    .get(header::CONTENT_LENGTH)
                    .is_some_and(|value| value == "0")
            {
                if let Some(lifecycle) = &details.stream_lifecycle {
                    lifecycle.complete();
                }
                app.record(
                    method.as_str(),
                    &path,
                    resp.status().as_u16(),
                    started,
                    details,
                )
                .await;
                return resp;
            }
            details.in_progress = true;
            // Some Codex upstream responses omit Content-Type despite sending
            // SSE. Retain the request's explicit SSE negotiation in that case.
            let is_sse = details.transport == "http_sse" || resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    value
                        .split(';')
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .eq_ignore_ascii_case("text/event-stream")
                });
            if is_sse {
                details.transport = "http_sse".into();
            }
            let metrics = logs::ResponseBodyMetrics::new(resp
                .headers()
                .get(header::CONTENT_ENCODING)
                .map(|value| value.to_str().unwrap_or("unsupported"))
                .unwrap_or_default());
            let remaining_bytes = resp
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok());
            let lifecycle = details.stream_lifecycle.clone();
            let diag_req = details.diag.clone();
            if let Some(req) = &diag_req {
                diag::emit(
                    "headers",
                    Some(req),
                    json!({
                        "status": resp.status().as_u16(),
                        "headerMs": details.response_header_ms,
                        "peer": details.peer_addr,
                        "http": details.http_version,
                        "returnedStateLen": details.returned_turn_state_len,
                        "transport": details.transport,
                    }),
                );
            }
            let injected_token = details.injected_token.clone();
            let entry = LogEntry::new(
                method.as_str(),
                &path,
                resp.status().as_u16(),
                started,
                details,
            );
            logs::push(&mut *app.logs.lock().await, entry.clone());
            let tracker = ResponseLogTracker {
                app,
                entry,
                started,
                metrics,
                finished: false,
                remaining_bytes,
                activity,
                lifecycle,
                diag: diag_req,
                diag_first_token: false,
                diag_finished: false,
                last_diag_chunks: 0,
                injected_token,
                mismatch_noted: false,
            };
            let (parts, body) = resp.into_parts();
            let stream = futures_util::stream::unfold(
                (body.into_data_stream(), tracker),
                move |(mut stream, mut tracker)| async move {
                    if tracker.finished {
                        return None;
                    }
                    let chunks = tracker
                        .lifecycle
                        .as_ref()
                        .map(|lifecycle| lifecycle.snapshot().stream_chunks)
                        .unwrap_or(0);
                    let next = match tokio::time::timeout(business_stream_idle_timeout(chunks), stream.next())
                        .await
                    {
                        Ok(next) => next,
                        Err(_) => {
                            if let Some(lifecycle) = &tracker.lifecycle {
                                lifecycle.error();
                            }
                            tracker.entry.error_kind = Some("stream_idle".into());
                            tracker.finished = true;
                            if is_sse {
                                let idle = Bytes::from_static(SSE_IDLE_TIMEOUT_EVENT);
                                tracker.metrics.observe(
                                    idle.as_ref(),
                                    tracker.started.elapsed().as_millis(),
                                    true,
                                );
                                Some(Ok(idle))
                            } else {
                                Some(Err(axum::Error::new(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "上游响应静默超时",
                                ))))
                            }
                        }
                    };
                    match &next {
                        Some(Ok(bytes)) => {
                            if let Some(lifecycle) = &tracker.lifecycle {
                                lifecycle.observe_chunk(bytes.len());
                            }
                            tracker.metrics.observe(
                                bytes,
                                tracker.started.elapsed().as_millis(),
                                is_sse,
                            );
                            // Hyper may stop polling immediately after Content-Length
                            // bytes. That is completion, not client cancellation.
                            if let Some(remaining) = &mut tracker.remaining_bytes {
                                *remaining = remaining.saturating_sub(bytes.len() as u64);
                                if *remaining == 0 {
                                    tracker
                                        .metrics
                                        .finish(tracker.started.elapsed().as_millis());
                                    if let Some(lifecycle) = &tracker.lifecycle {
                                        lifecycle.complete();
                                    }
                                    tracker.finished = true;
                                }
                            }
                        }
                        Some(Err(_)) => {
                            if let Some(lifecycle) = &tracker.lifecycle {
                                lifecycle.error();
                            }
                            tracker.entry.error_kind = Some("response_body".into());
                            tracker.finished = true;
                        }
                        None => {
                            tracker
                                .metrics
                                .finish(tracker.started.elapsed().as_millis());
                            if let Some(lifecycle) = &tracker.lifecycle {
                                lifecycle.complete();
                            }
                            tracker.finished = true;
                        }
                    }
                    if tracker.finished {
                        tracker.activity.take();
                        tracker.note_model_mismatch().await;
                    }
                    // No read-ahead or buffering for forwarding. Only publish metric changes
                    // and the final result; response bytes are passed through unchanged.
                    if tracker.finished
                        || tracker.entry.first_token_ms != tracker.metrics.first_token_ms()
                        || tracker.entry.output_tokens != tracker.metrics.output_tokens()
                        || tracker.entry.upstream_response_model.as_deref() != tracker.metrics.upstream_response_model()
                        || tracker.entry.error_kind.as_deref() != tracker.metrics.error_kind
                    {
                        tracker.refresh();
                        tracker.publish().await;
                    }
                    tracker.emit_diag_progress();
                    next.map(|item| (item, (stream, tracker)))
                },
            );
            Response::from_parts(parts, Body::from_stream(stream))
        }
        Err(err) => {
            let status = details.response_status
                .and_then(|status| StatusCode::from_u16(status).ok())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            app.record(method.as_str(), &path, status.as_u16(), started, details)
                .await;
            (status, err.to_string()).into_response()
        }
    }
}

fn fetch_account_is_current(settings: &Settings, account: &str) -> bool {
    login::chatgpt_credentials(Path::new(&settings.codex_home))
        .is_ok_and(|creds| creds.account_id == account)
}

struct ResponseLogTracker {
    app: Arc<App>,
    entry: LogEntry,
    started: Instant,
    metrics: logs::ResponseBodyMetrics,
    finished: bool,
    remaining_bytes: Option<u64>,
    activity: Option<RequestActivity>,
    lifecycle: Option<Arc<StreamLifecycle>>,
    diag: Option<diag::Request>,
    diag_first_token: bool,
    diag_finished: bool,
    last_diag_chunks: u64,
    injected_token: Option<String>,
    mismatch_noted: bool,
}

impl ResponseLogTracker {
    fn refresh(&mut self) {
        self.entry.ms = self.started.elapsed().as_millis();
        self.entry.first_token_ms = self.metrics.first_token_ms();
        self.entry.output_tokens = self.metrics.output_tokens();
        self.entry.upstream_response_model = self.metrics.upstream_response_model().map(str::to_owned);
        self.entry.in_progress = !self.finished;
        if self.entry.error_kind.is_none() {
            self.entry.error_kind = self.metrics.error_kind.map(str::to_owned);
        }
        self.entry.tokens_per_second = if self.finished && self.entry.error_kind.is_none() {
            self.entry
                .output_tokens
                .zip(self.entry.first_token_ms)
                .and_then(|(tokens, first)| {
                    let generation_ms = self.entry.ms.saturating_sub(first);
                    (generation_ms > 0).then(|| tokens as f64 * 1000.0 / generation_ms as f64)
                })
        } else {
            None
        };
    }

    async fn publish(&self) {
        replace_network_log(&self.app, self.entry.clone()).await;
    }

    async fn note_model_mismatch(&mut self) {
        if self.mismatch_noted || !self.metrics.completed() {
            return;
        }
        let Some(requested) = self.entry.model.clone() else {
            return;
        };
        let Some(upstream) = self.metrics.upstream_response_model().map(str::to_owned) else {
            return;
        };
        if upstream == requested {
            return;
        }
        self.mismatch_noted = true;
        self.app
            .observe_business_response(&requested, self.injected_token.as_deref(), false, Some(&upstream), true)
            .await;
    }

    fn emit_diag_progress(&mut self) {
        if self.diag.is_none() {
            return;
        }
        if !self.diag_first_token && self.metrics.first_token_ms().is_some() {
            self.diag_first_token = true;
            self.emit_diag("first_token");
        }
        let chunks = self
            .lifecycle
            .as_ref()
            .map(|lifecycle| lifecycle.snapshot().stream_chunks)
            .unwrap_or(self.entry.stream_chunks);
        if chunks > 0
            && chunks != self.last_diag_chunks
            && (chunks <= 8 || chunks % 20 == 0)
        {
            self.last_diag_chunks = chunks;
            self.emit_diag("chunk");
        }
        if self.finished && !self.diag_finished {
            self.diag_finished = true;
            self.emit_diag("finish");
        }
    }

    fn emit_diag(&self, stage: &str) {
        let Some(req) = &self.diag else {
            return;
        };
        let stream = self
            .lifecycle
            .as_ref()
            .map(|lifecycle| lifecycle.snapshot());
        diag::emit(
            stage,
            Some(req),
            json!({
                "status": self.entry.status,
                "streamState": stream.as_ref().map(|snapshot| snapshot.state).unwrap_or("not_tracked"),
                "chunks": stream.as_ref().map(|snapshot| snapshot.stream_chunks).unwrap_or(self.entry.stream_chunks),
                "bytes": stream.as_ref().map(|snapshot| snapshot.stream_bytes).unwrap_or(self.entry.stream_bytes),
                "firstTokenMs": self.entry.first_token_ms,
                "firstChunkMs": stream.as_ref().and_then(|snapshot| snapshot.first_chunk_ms),
                "lastChunkMs": stream.as_ref().and_then(|snapshot| snapshot.last_chunk_ms),
                "currentIdleMs": stream.as_ref().and_then(|snapshot| snapshot.current_idle_ms),
                "maxIdleMs": stream.as_ref().and_then(|snapshot| snapshot.max_idle_ms),
                "error": self.entry.error_kind,
                "events": self.metrics.sse_event_summary(),
                "inProgress": !self.finished,
                "peer": self.entry.peer_addr,
                "http": self.entry.http_version,
                "returnedStateLen": self.entry.returned_turn_state_len,
            }),
        );
    }
}

async fn replace_network_log(app: &App, entry: LogEntry) {
    if let Some(existing) = app
        .logs
        .lock()
        .await
        .iter_mut()
        .find(|existing| existing.id == entry.id)
    {
        *existing = entry;
    }
}

impl Drop for ResponseLogTracker {
    fn drop(&mut self) {
        if !self.finished {
            if self.metrics.completed() {
                if let Some(lifecycle) = &self.lifecycle {
                    lifecycle.complete();
                }
            } else if self.metrics.error_kind.is_none() {
                if let Some(lifecycle) = &self.lifecycle {
                    lifecycle.cancel();
                }
                self.entry.error_kind = Some("client_cancelled".into());
            } else if let Some(lifecycle) = &self.lifecycle {
                lifecycle.error();
            }
            self.finished = true;
            self.refresh();
        }
        if !self.diag_finished {
            self.diag_finished = true;
            self.emit_diag("finish");
        }
        // A disconnected client drops the body without polling EOF. Preserve that
        // partial request too, without continuing to read the upstream response.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let app = self.app.clone();
            let entry = self.entry.clone();
            runtime.spawn(async move {
                replace_network_log(&app, entry).await;
            });
        }
    }
}

fn resolved_business_proxy(
    settings: &Settings,
    warp: &WarpRuntime,
    mihomo: &MihomoRuntime,
) -> String {
    if !settings.same_network() {
        return settings.upstream_proxy.clone();
    }
    match settings.outbound_mode {
        OutboundMode::Warp => warp.proxy_url().unwrap_or_default(),
        OutboundMode::Mihomo => mihomo.proxy_url().unwrap_or_default(),
        OutboundMode::Manual => settings.outbound_proxy.clone(),
    }
}

fn business_network_details(settings: &Settings, upstream: &str, proxy: &str) -> NetworkLogDetails {
    let mut details = logs::network_details(upstream, proxy);
    if settings.same_network() && !proxy.trim().is_empty() {
        details.route_kind = match settings.outbound_mode {
            OutboundMode::Warp => logs::ROUTE_EMBEDDED_WARP.into(),
            OutboundMode::Mihomo => logs::ROUTE_EMBEDDED_MIHOMO.into(),
            OutboundMode::Manual => logs::ROUTE_MANUAL_PROXY.into(),
        };
    }
    details
}

fn business_http_client(template: &str, session: Option<&str>) -> Result<reqwest::Client> {
    let proxy = match fetch::apply_bound_session(template, session) {
        Ok(proxy) => proxy,
        Err(_) if fetch::has_session_placeholder(template) => {
            fetch::replace_session_placeholder(template, "unbound0")
        }
        Err(err) => return Err(err),
    };
    upstream_http_client(&proxy)
}

fn upstream_http_client(proxy: &str) -> Result<reqwest::Client> {
    let proxy = crate::settings::normalize_proxy(proxy, "上游转发代理")?;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(5));
    if !proxy.is_empty() {
        let proxy = reqwest::Proxy::all(fetch::outbound_proxy_for_client(&proxy))
            .map_err(|_| anyhow::anyhow!("上游转发代理地址无效"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|_| anyhow::anyhow!("无法创建上游转发客户端"))
}

#[cfg(test)]
async fn forward_http(app: &App, req: Request<Body>) -> Result<Response> {
    let mut details = NetworkLogDetails::default();
    forward_http_with_log(app, req, &mut details, Instant::now()).await
}

#[cfg(test)]
async fn forward_http_with_log(
    app: &App,
    req: Request<Body>,
    details: &mut NetworkLogDetails,
    started: Instant,
) -> Result<Response> {
    let response = forward_http_tracked(app, req, details, &mut None, started).await?;
    let Some(lifecycle) = details.stream_lifecycle.clone() else {
        return Ok(response);
    };
    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(
        parts,
        Body::from_stream(ObservedStream::new(body.into_data_stream(), lifecycle)),
    ))
}

fn state_wait_error(details: &mut NetworkLogDetails, status: StatusCode, kind: &str, message: &str) -> anyhow::Error {
    details.response_status = Some(status.as_u16());
    details.error_kind = Some(kind.into());
    details.turn_state_action = kind.into();
    anyhow::anyhow!("{message}")
}

/// Wait inside the request future: cancellation drops this waiter, and only the
/// existing fetch loop performs probes (including its shared rate limits).
async fn wait_for_request_state(
    app: &App,
    request_settings: &Settings,
    account: Option<&str>,
    model: Option<&str>,
    details: &mut NetworkLogDetails,
) -> Result<String> {
    let Some(model) = model else {
        return Err(state_wait_error(details, StatusCode::UNPROCESSABLE_ENTITY, "state_model_unknown", "无法识别请求模型，不能等待匹配的 state；请求未转发"));
    };
    let Some(account) = account else {
        return Err(state_wait_error(details, StatusCode::CONFLICT, "state_account_unknown", "无法识别请求账号，不能等待匹配的 state；请求未转发"));
    };
    app.model_notify.notify_one();
    loop {
        let current = app.settings.lock().await.clone();
        if current.state_miss_policy != StateMissPolicy::Wait
            || current.token_reuse_policy != request_settings.token_reuse_policy {
            return Err(state_wait_error(details, StatusCode::CONFLICT, "state_wait_policy_changed", "等待策略已切换，请重新发起请求；请求未转发"));
        }
        if current.codex_home != request_settings.codex_home || current.upstream != request_settings.upstream
            || current.upstream_proxy != request_settings.upstream_proxy || current.outbound_proxy != request_settings.outbound_proxy
            || current.outbound_mode != request_settings.outbound_mode || current.warp_http2 != request_settings.warp_http2
            || current.network_route_policy != request_settings.network_route_policy
            || current.forced_model != request_settings.forced_model {
            return Err(state_wait_error(details, StatusCode::CONFLICT, "state_wait_config_changed", "等待期间线路配置已变化，请重新发起请求；请求未转发"));
        }
        if !login::chatgpt_credentials(Path::new(&request_settings.codex_home)).is_ok_and(|creds| creds.account_id == account) {
            return Err(state_wait_error(details, StatusCode::CONFLICT, "state_wait_account_changed", "等待期间登录账号已变化或退出，请重新发起请求；请求未转发"));
        }
        {
            let mut store = app.turn_state.lock().await;
            if store.is_bound_to_account(account) {
                // Keep the model active while its callers wait, including after
                // the background loop has initialized a newly logged-in account.
                if store.register_model(model) {
                    app.model_notify.notify_one();
                }
                if let Some(token) = store.peek_for_model(model) {
                    return Ok(token);
                }
            }
        }
        if current.token_fetch_paused {
            return Err(state_wait_error(details, StatusCode::CONFLICT, "state_wait_fetch_paused", "已暂停获取 Token，当前没有可用凭证；请求未转发"));
        }
        // Poll also observes login files changed outside Kit. No locks are held
        // while sleeping and no task is spawned that could outlive the client.
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(250)) => {},
            _ = app.fetch_change_notify.notified() => {},
        }
    }
}

async fn forward_http_tracked(
    app: &App,
    req: Request<Body>,
    details: &mut NetworkLogDetails,
    activity: &mut Option<RequestActivity>,
    started: Instant,
) -> Result<Response> {
    let (upstream, home, upstream_proxy, cached_http, state_miss_policy, request_settings) = {
        let settings = app.settings.lock().await;
        let business_proxy = resolved_business_proxy(&settings, &app.warp, &app.mihomo);
        (
            settings.upstream.clone(),
            settings.codex_home.clone(),
            business_proxy,
            app.http.lock().await.clone(),
            settings.state_miss_policy,
            settings.clone(),
        )
    };
    let effective_proxy = fetch::outbound_proxy_for_client(&upstream_proxy);
    *details = business_network_details(&request_settings, &upstream, &effective_proxy);
    if request_settings.same_network()
        && request_settings.outbound_mode == OutboundMode::Warp
        && upstream_proxy.trim().is_empty()
    {
        anyhow::bail!(
            "{}",
            app.warp.proxy_url().err().map(|err| err.to_string()).unwrap_or_else(|| {
                "内置 WARP 正在自动连接，请稍候。".into()
            })
        );
    }
    if request_settings.same_network()
        && request_settings.outbound_mode == OutboundMode::Mihomo
        && upstream_proxy.trim().is_empty()
    {
        anyhow::bail!(
            "{}",
            app.mihomo
                .proxy_url()
                .err()
                .map(|err| err.to_string())
                .unwrap_or_else(|| "订阅节点正在连接，请稍候。".into())
        );
    }
    details.state_policy = Some(state_miss_policy);
    let (mut parts, body) = req.into_parts();
    let target = join_upstream(&upstream, &parts.uri)?;
    let path = parts.uri.path();

    // 先读取 body，以便从中提取或改写 model 字段
    let mut bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .context("read body")?;
    details.body_bytes = bytes.len();

    let compacting = turn_state::is_context_compaction(path, &bytes);
    let should_stamp = turn_state::should_stamp_http(parts.method.as_str(), path) && !compacting;
    let content_encoding = parts
        .headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|value| value.to_string());
    details.content_encoding = logs::safe_content_encoding(content_encoding.as_deref());
    if should_stamp {
        if let Some(forced) = request_settings.forced_model() {
            bytes = turn_state::rewrite_model_in_body(&bytes, content_encoding.as_deref(), forced)
                .map_err(|err| anyhow::anyhow!("{err}"))?
                .into();
            details.body_bytes = bytes.len();
        }
    }
    details.transport = if parts
        .headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"))
    {
        "http_sse".into()
    } else {
        "http".into()
    };
    debug_log(&format!(
        "[proxy] {} {} body={}bytes encoding={} should_stamp={}",
        parts.method,
        path,
        bytes.len(),
        details.content_encoding,
        should_stamp
    ));

    let client_account = request_account(&parts.headers);
    let request_identity = app.sync_request_identity(Path::new(&home)).await;
    let client_credentials_conflict = request_identity
        .as_ref()
        .is_some_and(|(creds, _)| login::credentials_conflict_headers(&parts.headers, creds));
    let request_account_matches = request_identity
        .as_ref()
        .is_some_and(|(creds, override_headers)| {
            *override_headers || login::credentials_match_headers(&parts.headers, creds)
        });
    if let Some((creds, true)) = &request_identity {
        if !login::apply_chatgpt_credentials_headers(&mut parts.headers, creds) {
            anyhow::bail!("无法应用 ChatGPT 鉴权请求头");
        }
    }
    let effective_account = request_account(&parts.headers);
    let account_changed = request_identity
        .as_ref()
        .is_some_and(|(creds, override_headers)| {
            *override_headers && client_account.as_deref() != Some(creds.account_id.as_str())
        });
    details.account_id = effective_account.as_deref().map(|id| logs::safe_text(id, 128));
    if let Some((creds, _)) = &request_identity {
        if effective_account.as_deref() == Some(creds.account_id.as_str()) {
            details.account_email = creds
                .email
                .as_deref()
                .map(|email| logs::safe_text(email, 254));
        }
    }
    let request_model = should_stamp
        .then(|| turn_state::extract_model_from_body(&bytes))
        .flatten();
    let same_turn = should_stamp && turn_state::is_same_turn_follow_up(&bytes);
    let mut client_had_state = false;
    let mut injected_token: Option<String> = None;
    if should_stamp {
        details.model = request_model
            .as_deref()
            .map(|model| logs::safe_text(model, 80))
            .filter(|model| !model.is_empty());
        if state_miss_policy == StateMissPolicy::Wait && client_credentials_conflict {
            return Err(state_wait_error(
                details,
                StatusCode::CONFLICT,
                "state_account_mismatch",
                "账号凭据不匹配，请同步 Codex 登录账号后重试；请求未转发",
            ));
        }
        if request_model.is_none() {
            debug_log(&format!(
                "[proxy] 未能解析 model，body={}bytes encoding={}",
                bytes.len(),
                details.content_encoding
            ));
        }

        // 被动发现：从请求中提取模型，自动注册到 token 池
        if let Some(ref model) = request_model {
            let is_new = app.turn_state.lock().await.register_model(model);
            if is_new {
                debug_log(&format!("[discover] 发现新模型: {}，通知 fetch 循环预取 token", model));
                app.model_notify.notify_one();
            }
        }

        let client_already_has = turn_state::has_http_turn_state(&parts.headers)
            || turn_state::has_body_turn_state(&bytes);
        client_had_state = client_already_has;
        if state_miss_policy == StateMissPolicy::StripAll {
            parts.headers.remove(turn_state::HEADER_NAME);
            details.turn_state_action = "removed_all_policy".into();
        } else if state_miss_policy == StateMissPolicy::Passthrough {
            details.turn_state_action = if client_already_has { "preserved_by_policy" } else { "initial_request" }.into();
            details.turn_state_len = parts.headers.get(turn_state::HEADER_NAME)
                .and_then(|value| value.to_str().ok()).map(str::trim).filter(|value| !value.is_empty()).map(str::len);
        } else {
            // 探针当作每轮第一包。新一轮首包补上规范票据并包装成同轮第二包；
            // 同轮续跑只改请求头，保留客户端 body（含 previous_response_id）。
            let mut token = {
                let store = app.turn_state.lock().await;
                if request_account_matches && effective_account.as_deref().is_some_and(|id| store.is_bound_to_account(id)) {
                    request_model
                        .as_deref()
                        .and_then(|model| store.peek_for_model(model))
                        .and_then(|value| turn_state::injectable_http_token(&value))
                } else {
                    None
                }
            };
            let waited = token.is_none() && state_miss_policy == StateMissPolicy::Wait;
            if waited {
                token = turn_state::injectable_http_token(
                    &wait_for_request_state(
                        app,
                        &request_settings,
                        effective_account.as_deref(),
                        request_model.as_deref(),
                        details,
                    )
                    .await?,
                );
            }
            if let Some(token) = token {
                turn_state::apply_http_header(&mut parts.headers, &token);
                // 同轮续跑（带 previous_response_id 或 tool output）只换请求头。
                // 客户端往往不回带 State；若仍包装第二包会丢掉 previous_response_id，
                // 子代理多步后续跑会空转推理。
                let header_only = same_turn;
                if !header_only {
                    match turn_state::wrap_as_second_packet(&bytes, content_encoding.as_deref(), &token)
                    {
                        Ok(wrapped) => {
                            bytes = wrapped.into();
                            details.body_bytes = bytes.len();
                        }
                        Err(err) => {
                            eprintln!("[stamp] 包装同轮第二包失败，仅写入请求头: {err}");
                        }
                    }
                }
                details.turn_state_action = if header_only {
                    if waited {
                        "header_only_after_wait".into()
                    } else {
                        "header_only".into()
                    }
                } else {
                    match (client_already_has, waited) {
                        (true, true) => "replaced_after_wait".into(),
                        (true, false) => "replaced".into(),
                        (false, true) => "injected_after_wait".into(),
                        (false, false) => "injected".into(),
                    }
                };
                details.turn_state_len = Some(token.len());
                eprintln!(
                    "[stamp] {} turn_state → token len={} model={:?} body={} 到 {} {}",
                    if header_only {
                        "只改请求头"
                    } else if client_already_has {
                        "替换"
                    } else {
                        "补上"
                    },
                    token.len(),
                    request_model,
                    if header_only { "原样" } else { "包装第二包" },
                    parts.method,
                    path
                );
                injected_token = Some(token);
            } else if client_already_has {
                if state_miss_policy == StateMissPolicy::Strip {
                    parts.headers.remove(turn_state::HEADER_NAME);
                    details.turn_state_action = "removed_by_policy".into();
                } else if account_changed {
                    parts.headers.remove(turn_state::HEADER_NAME);
                    details.turn_state_action = "removed_account_mismatch".into();
                } else {
                    details.turn_state_action = match (&request_model, request_account_matches) {
                        (None, _) => "preserved_unknown_model".into(),
                        (Some(_), false) => "preserved_account_mismatch".into(),
                        (Some(_), true) => "preserved_no_ticket".into(),
                    };
                    details.turn_state_len = parts
                        .headers
                        .get(turn_state::HEADER_NAME)
                        .and_then(|value| value.to_str().ok())
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::len);
                    eprintln!(
                        "[stamp] 无可用 token（model={:?}），保留客户端原值 {} {}",
                        request_model, parts.method, path
                    );
                }
            } else {
                details.turn_state_action = "initial_request".into();
                eprintln!(
                    "[stamp] 客户端未携带 State，且没有可注入的规范票据 {} {} model={:?}",
                    parts.method, path, request_model
                );
            }
        }
    } else {
        details.turn_state_action = if compacting {
            "compaction_passthrough".into()
        } else {
            "not_applicable".into()
        };
        if state_miss_policy == StateMissPolicy::StripAll {
            parts.headers.remove(turn_state::HEADER_NAME);
            details.turn_state_action = "removed_all_policy".into();
        }
    }
    let (bound_session, routing_cookies) = {
        let store = app.turn_state.lock().await;
        (
            store.proxy_session_for_token(injected_token.as_deref()),
            injected_token
                .as_deref()
                .map(|token| store.routing_cookies_for_token(Some(token)))
                .unwrap_or_default(),
        )
    };
    details.proxy_session = bound_session.clone();
    details.diag = Some(diag::Request {
        id: diag::next_id(),
        flow: details.flow.clone(),
        model: details.model.clone(),
        same_turn,
        client_had_state,
        token_fp: injected_token.as_deref().map(diag::token_fp),
        token_age_secs: injected_token.as_deref().and_then(diag::token_age_secs),
        cookies: chatgpt_cookies::cookie_names(&routing_cookies),
        turn_state_action: details.turn_state_action.clone(),
        route_kind: details.route_kind.clone(),
        proxy_session: bound_session.clone(),
    });
    if let Some(req) = &details.diag {
        diag::emit(
            "request",
            Some(req),
            json!({
                "method": parts.method.as_str(),
                "path": path,
                "bodyBytes": details.body_bytes,
                "injectedLen": injected_token.as_ref().map(|token| token.len()),
            }),
        );
    }
    details.injected_token = injected_token.clone();
    if injected_token.is_some() {
        let chatgpt_host = chatgpt_cookies::is_chatgpt_https_url(&target);
        if !routing_cookies.is_empty() || chatgpt_host {
            chatgpt_cookies::apply_to_headers(&mut parts.headers, &routing_cookies, chatgpt_host);
            let names = chatgpt_cookies::cookie_names(&routing_cookies);
            if !names.is_empty() {
                debug_log(&format!("[stamp] 回放线路 cookie {}", names.join(",")));
            }
        }
    }
    let http = if fetch::has_session_placeholder(&upstream_proxy) {
        let resolved = fetch::apply_bound_session(&upstream_proxy, bound_session.as_deref())?;
        details.proxy_endpoint = Some(logs::endpoint_origin(&resolved));
        business_http_client(&upstream_proxy, bound_session.as_deref())?
    } else {
        cached_http
    };
    let mut builder = http
        .request(
            reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?,
            target,
        )
        .body(bytes);
    for (name, value) in &parts.headers {
        if is_hop(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    if let Some(account) = parts
        .headers
        .get("chatgpt-account-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        *activity = Some(app.traffic.begin(account, Instant::now()));
    }
    let upstream_resp = match builder.send().await {
        Ok(response) => response,
        Err(error) => {
            details.error_kind = Some(logs::request_error_kind(&error));
            return Err(error).context("upstream http");
        }
    };
    let response_header_ms = started.elapsed().as_millis();
    details.response_header_ms = Some(response_header_ms);
    if let Some(content_type) = upstream_resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    {
        details.transport = if content_type
            .to_ascii_lowercase()
            .contains("text/event-stream")
        {
            "http_sse".into()
        } else {
            "http".into()
        };
    }
    let resp_status_u16 = upstream_resp.status().as_u16();
    details.peer_addr = upstream_resp.remote_addr().map(|addr| addr.to_string());
    details.final_origin = Some(logs::endpoint_origin(upstream_resp.url().as_str()));
    details.http_version = Some(
        match upstream_resp.version() {
            Version::HTTP_09 => "HTTP/0.9",
            Version::HTTP_10 => "HTTP/1.0",
            Version::HTTP_11 => "HTTP/1.1",
            Version::HTTP_2 => "HTTP/2",
            Version::HTTP_3 => "HTTP/3",
            _ => "HTTP/unknown",
        }
        .into(),
    );

    // 记录上游响应详情，方便排查 token 失效
    let upstream_turn_state = turn_state::header_token(upstream_resp.headers());
    details.returned_turn_state_len = upstream_turn_state.as_ref().map(|token| token.len());
    if let Some(model) =
        degraded_response_model(request_model.as_deref(), upstream_turn_state.as_deref())
    {
        app.observe_business_response(
            model,
            injected_token.as_deref(),
            true,
            None,
            false,
        )
        .await;
    }
    let injected_len = injected_token.as_ref().map(|t| t.len());
    let upstream_ts_len = upstream_turn_state.as_ref().map(|t| t.len());
    let same_token = match (&injected_token, &upstream_turn_state) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };
    eprintln!(
        "[resp] {} {} → {} | 注入={}字节 上游返回={}字节 same={}",
        parts.method,
        path,
        resp_status_u16,
        injected_len
            .map(|l| l.to_string())
            .unwrap_or_else(|| "无".into()),
        upstream_ts_len
            .map(|l| l.to_string())
            .unwrap_or_else(|| "无".into()),
        same_token
    );

    let status = StatusCode::from_u16(resp_status_u16)?;
    let mut headers = HeaderMap::new();
    for (name, value) in upstream_resp.headers() {
        if is_hop(name) {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_ref()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            headers.append(n, v);
        }
    }
    if details.transport == "http_sse" {
        let lifecycle = Arc::new(StreamLifecycle::new(started, response_header_ms));
        details.stream_lifecycle = Some(lifecycle.clone());
    }
    let body = Body::from_stream(upstream_resp.bytes_stream());
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

// WebSocket 代理已移除 — 所有 WS 升级请求在 proxy() 入口返回 426，
// 触发 Codex CLI 自动切换到 HTTP SSE 模式。
// 这保证了所有请求都经过 proxy_http()，可以可靠地提取 model 并注入对应 token。

fn is_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP
        .iter()
        .any(|h| name.as_str().eq_ignore_ascii_case(h))
}

fn request_account(headers: &HeaderMap) -> Option<String> {
    headers.get("chatgpt-account-id").and_then(|value| value.to_str().ok())
        .map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

pub fn join_upstream(upstream: &str, uri: &Uri) -> Result<String> {
    let mut base = upstream.trim().to_string();
    if !base.ends_with('/') {
        base.push('/');
    }
    let mut url = Url::parse(&base).context("upstream url")?;
    let path = uri.path().trim_start_matches('/');
    url = url.join(path).context("join path")?;
    url.set_query(uri.query());
    Ok(url.to_string())
}

#[cfg(test)]
#[path = "upstream_proxy_tests.rs"]
mod upstream_proxy_tests;

#[cfg(test)]
#[path = "account_switch_tests.rs"]
mod account_switch_tests;

#[cfg(test)]
#[path = "state_policy_tests.rs"]
mod state_policy_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;

    fn ticket_for_len(target_len: usize) -> String {
        let mut raw = vec![0_u8; target_len * 3 / 4];
        raw[0] = 0x80;
        raw[1..9].copy_from_slice(&(chrono::Utc::now().timestamp() as u64).to_be_bytes());
        URL_SAFE_NO_PAD.encode(raw)
    }

    #[test]
    fn warp_route_never_falls_back_to_saved_manual_proxy() {
        let settings = Settings {
            outbound_mode: OutboundMode::Warp,
            outbound_proxy: "http://127.0.0.1:7890".into(),
            ..Settings::default()
        };
        let app = App::new(settings.clone()).unwrap();
        assert!(app.fetch_settings(&settings).is_err());
        let mut manual = settings;
        manual.outbound_mode = OutboundMode::Manual;
        assert_eq!(
            app.fetch_settings(&manual).unwrap().outbound_proxy,
            "http://127.0.0.1:7890"
        );
    }

    #[test]
    fn joins_path_and_query() {
        let uri: Uri = "http://127.0.0.1:8787/responses?foo=1".parse().unwrap();
        let out = join_upstream("https://chatgpt.com/backend-api/codex", &uri).unwrap();
        assert_eq!(out, "https://chatgpt.com/backend-api/codex/responses?foo=1");
    }

    #[test]
    fn joins_nested_path() {
        let uri: Uri = "http://127.0.0.1:8787/v1/responses".parse().unwrap();
        let out = join_upstream("https://chatgpt.com/backend-api/codex/", &uri).unwrap();
        assert_eq!(out, "https://chatgpt.com/backend-api/codex/v1/responses");
    }

    #[test]
    fn shared_fetch_round_groups_donors_without_starving_other_bindings() {
        let mut store = TurnStateStore::default();
        let models = vec!["a".into(), "b".into(), "independent".into()];
        store.set_model_bound_len("independent", Some(332));
        for round in 1..=20 {
            let selected = model_for_reuse_round(&store, &models, round).unwrap();
            if round % 2 == 0 {
                assert_eq!(selected, "independent");
            } else {
                assert!(["a", "b"].contains(&selected));
            }
        }
        store.set_reuse_policy(TokenReusePolicy::PerModel);
        for round in 1..=6 {
            assert_eq!(model_for_reuse_round(&store, &models, round), model_for_fetch_round(&models, round));
        }
        assert!(model_for_reuse_round(&store, &[], 1).is_none());
        store.set_reuse_policy(TokenReusePolicy::Shared292);
        let pinned = pin_shared_donor(&store, &models, "b");
        assert_eq!(pinned, vec!["b".to_string(), "independent".to_string()]);
        for round in 1..=10 {
            let selected = model_for_reuse_round(&store, &pinned, round).unwrap();
            if round % 2 == 0 {
                assert_eq!(selected, "independent");
            } else {
                assert_eq!(selected, "b");
            }
        }
        let only_donor = pin_shared_donor(&store, &["a".into(), "b".into()], "gpt-5.5");
        assert_eq!(only_donor, vec!["gpt-5.5".to_string()]);
        assert!(only_donor.iter().all(|model| model == "gpt-5.5"));
    }

    #[test]
    fn fetch_round_rotates_models_without_starvation() {
        let models = vec!["astra".to_string(), "sol".to_string(), "other".to_string()];
        let selected: Vec<_> = (1..=5)
            .map(|round| model_for_fetch_round(&models, round).unwrap())
            .collect();
        assert_eq!(selected, vec!["astra", "sol", "other", "astra", "sol"]);
        assert_eq!(model_for_fetch_round(&[], 1), None);
    }

    #[test]
    fn fetch_errors_never_retry_without_delay() {
        assert_eq!(fetch_retry_delay(FetchRetryClass::Normal), fetch::RETRY_INTERVAL);
        assert_eq!(fetch_retry_delay(FetchRetryClass::Backoff), fetch::ERROR_BACKOFF);
        assert_eq!(fetch_retry_delay(FetchRetryClass::Auth), fetch::AUTH_BACKOFF);
        assert_eq!(
            fetch_retry_delay(FetchRetryClass::Forbidden),
            fetch::FORBIDDEN_BACKOFF
        );
        assert_eq!(fetch_retry_delay(FetchRetryClass::Stale), fetch::RETRY_INTERVAL);
        assert!(fetch::RETRY_INTERVAL >= Duration::from_secs(6));
        assert_eq!(fetch::MAX_FETCH_BURST, 4);
        assert_eq!(fetch::fetch_burst_concurrency(0), 1);
        assert_eq!(fetch::fetch_burst_concurrency(2), 4);
        assert_eq!(
            length_miss_rest(4, FetchRetryClass::Normal, true),
            Some(fetch::BURST_EXHAUSTED_BACKOFF)
        );
        assert_eq!(length_miss_rest(2, FetchRetryClass::Normal, true), None);
        assert_eq!(length_miss_rest(4, FetchRetryClass::Backoff, true), None);
        assert_eq!(length_miss_rest(4, FetchRetryClass::Normal, false), None);
    }

    #[tokio::test]
    async fn forbidden_model_backoff_does_not_block_a_different_model() {
        let app = App::new(Settings::default()).unwrap();
        let forbidden = classify_fetch_failure(&NetworkLogDetails {
            response_status: Some(403),
            ..NetworkLogDetails::default()
        });
        app.defer_fetch_failure("luna", forbidden).await;

        let models = vec!["astra".to_string(), "luna".to_string()];
        let (eligible, wait) = app.eligible_fetch_models(&models).await;
        assert_eq!(eligible, vec!["astra"]);
        assert!(wait <= fetch::RETRY_INTERVAL);

        // A direct call for the cooled model is rejected before route or
        // network work and does not clear the other model's eligibility.
        let err = app.fetch_once("luna").await.unwrap_err();
        assert_eq!(err.retry, FetchRetryClass::Deferred);
        let (eligible, _) = app.eligible_fetch_models(&models).await;
        assert_eq!(eligible, vec!["astra"]);
    }

    #[tokio::test]
    async fn request_identity_rebinds_pool_before_header_override() {
        if std::env::var_os("CSK_IDENTITY_TEST_CHILD").is_none() {
            let dir = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proxy::tests::request_identity_rebinds_pool_before_header_override",
                    "--nocapture",
                ])
                .env("CSK_IDENTITY_TEST_CHILD", "1")
                .env("HOME", dir.path())
                .env("USERPROFILE", dir.path())
                .env("APPDATA", dir.path())
                .env("LOCALAPPDATA", dir.path())
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let home = crate::settings::home_dir().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            login::kit_auth_path(&home),
            r#"{
  "auth_mode": "chatgpt",
  "tokens": {
    "access_token": "new-access",
    "refresh_token": "new-refresh",
    "account_id": "new-account"
  }
}"#,
        )
        .unwrap();

        let app = App::new(Settings {
            codex_home: home.display().to_string(),
            ..Settings::default()
        })
        .unwrap();
        {
            let mut store = app.turn_state.lock().await;
            store.bind_account("old-account");
            store.register_model("gpt-6-astra");
            let old_ticket = ticket_for_len(turn_state::QUALITY_TOKEN_LEN);
            assert!(store.capture("gpt-6-astra", &old_ticket, "test"));
        }

        let (creds, override_headers) = app.sync_request_identity(&home).await.unwrap();
        assert!(override_headers);
        assert_eq!(creds.account_id, "new-account");
        let current_ticket = ticket_for_len(turn_state::QUALITY_TOKEN_LEN);
        {
            let mut store = app.turn_state.lock().await;
            assert!(store.is_bound_to_account("new-account"));
            assert!(store.peek_for_model("gpt-6-astra").is_none());
            store.register_model("gpt-6-astra");
            assert!(store.capture("gpt-6-astra", &current_ticket, "test"));
        }
        assert!(app
            .turn_state
            .lock()
            .await
            .peek_for_model("gpt-6-astra")
            .as_deref()
            == Some(current_ticket.as_str()));
        app.observe_business_response(
            "gpt-6-astra",
            Some(&current_ticket),
            true,
            None,
            false,
        )
        .await;
        assert!(app
            .turn_state
            .lock()
            .await
            .peek_for_model("gpt-6-astra")
            .is_none());

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer old-access"),
        );
        assert!(!login::credentials_match_headers(&headers, &creds));
        login::apply_chatgpt_credentials_headers(&mut headers, &creds);
        assert!(login::credentials_match_headers(&headers, &creds));
    }

    #[tokio::test]
    async fn unauthorized_and_connect_backoffs_remain_global() {
        let cases = [
            NetworkLogDetails {
                response_status: Some(401),
                ..NetworkLogDetails::default()
            },
            NetworkLogDetails {
                error_kind: Some("connect".into()),
                ..NetworkLogDetails::default()
            },
        ];
        for details in cases {
            let app = App::new(Settings::default()).unwrap();
            let class = classify_fetch_failure(&details);
            app.defer_fetch_failure("luna", class).await;
            let models = vec!["astra".to_string(), "luna".to_string()];
            let (eligible, wait) = app.eligible_fetch_models(&models).await;
            assert_eq!(eligible, models);
            assert!(wait >= fetch_retry_delay(class).saturating_sub(Duration::from_secs(1)));
        }
    }

    #[tokio::test]
    async fn all_model_backoffs_wait_for_the_earliest_deadline() {
        let app = App::new(Settings::default()).unwrap();
        let now = Instant::now();
        {
            let mut delays = app.fetch_model_next_allowed_at.lock().await;
            delays.insert("astra".into(), now + Duration::from_millis(80));
            delays.insert("luna".into(), now + Duration::from_millis(160));
        }
        let models = vec!["astra".to_string(), "luna".to_string()];
        let (eligible, wait) = app.eligible_fetch_models(&models).await;
        assert!(eligible.is_empty());
        assert!(wait >= Duration::from_millis(50));
        assert!(wait <= Duration::from_millis(100));
    }

    #[tokio::test]
    async fn long_model_backoff_still_rechecks_other_models_periodically() {
        let app = App::new(Settings::default()).unwrap();
        app.fetch_model_next_allowed_at
            .lock()
            .await
            .insert("luna".into(), Instant::now() + fetch::AUTH_BACKOFF);
        let (eligible, wait) = app.eligible_fetch_models(&["luna".into()]).await;
        assert!(eligible.is_empty());
        assert!(wait <= fetch::CHECK_INTERVAL);
        assert!(wait >= fetch::CHECK_INTERVAL.saturating_sub(Duration::from_secs(1)));
    }

    #[tokio::test]
    async fn resetting_fetch_schedule_clears_model_backoffs() {
        let app = App::new(Settings::default()).unwrap();
        app.defer_fetch_failure("luna", FetchRetryClass::Forbidden)
            .await;
        app.reset_fetch_schedule().await;
        let models = vec!["astra".to_string(), "luna".to_string()];
        let (eligible, _) = app.eligible_fetch_models(&models).await;
        assert_eq!(eligible, models);
    }

    #[test]
    fn fetch_failure_class_uses_structured_status_and_error_kind() {
        let unauthorized = NetworkLogDetails {
            response_status: Some(401),
            ..NetworkLogDetails::default()
        };
        assert_eq!(
            classify_fetch_failure(&unauthorized),
            FetchRetryClass::Auth
        );
        let forbidden = NetworkLogDetails {
            response_status: Some(403),
            ..NetworkLogDetails::default()
        };
        assert_eq!(
            classify_fetch_failure(&forbidden),
            FetchRetryClass::Forbidden
        );
        for status in [429, 503] {
            let details = NetworkLogDetails {
                response_status: Some(status),
                ..NetworkLogDetails::default()
            };
            assert_eq!(classify_fetch_failure(&details), FetchRetryClass::Backoff);
        }
        let connect = NetworkLogDetails {
            error_kind: Some("connect".into()),
            ..NetworkLogDetails::default()
        };
        assert_eq!(classify_fetch_failure(&connect), FetchRetryClass::Backoff);
        assert_eq!(
            classify_fetch_failure(&NetworkLogDetails::default()),
            FetchRetryClass::Normal
        );
    }

    #[test]
    fn degraded_response_requires_a_model_and_degraded_length_ticket() {
        let degraded = format!("gAAAAA{}", "x".repeat(turn_state::DEGRADED_TOKEN_LEN - 6));
        let quality = format!("gAAAAA{}", "x".repeat(turn_state::QUALITY_TOKEN_LEN - 6));
        assert_eq!(
            degraded_response_model(Some("gpt-6-astra"), Some(&degraded)),
            Some("gpt-6-astra")
        );
        assert_eq!(
            degraded_response_model(Some("gpt-6-astra"), Some(&quality)),
            None
        );
        assert_eq!(degraded_response_model(None, Some(&degraded)), None);
        assert_eq!(degraded_response_model(Some("gpt-6-astra"), None), None);
    }

    #[test]
    fn fetched_ticket_must_match_the_models_exact_bound_length() {
        let mut store = TurnStateStore::default();
        store.set_model_bound_len("astra", Some(turn_state::QUALITY_TOKEN_LEN));
        let original_292 = ticket_for_len(turn_state::QUALITY_TOKEN_LEN);
        assert!(capture_fetched_ticket(&mut store, "astra", &original_292, None, None, &[]));
        let token_332 = ticket_for_len(turn_state::QUALITY_TOKEN_LEN_332);
        assert!(!capture_fetched_ticket(&mut store, "astra", &token_332, None, None, &[]));
        assert_eq!(
            store.peek_for_model("astra").as_deref(),
            Some(original_292.as_str())
        );

        let token_292 = ticket_for_len(turn_state::QUALITY_TOKEN_LEN);
        assert!(capture_fetched_ticket(&mut store, "astra", &token_292, None, None, &[]));
        assert_eq!(store.peek_for_model("astra").as_deref(), Some(token_292.as_str()));

        let mut auto = TurnStateStore::default();
        assert!(capture_fetched_ticket(&mut auto, "astra", &token_332, None, None, &[]));
        assert_eq!(auto.bound_len_for("astra"), turn_state::QUALITY_TOKEN_LEN_332);
    }

    #[tokio::test]
    async fn shared_fetch_slot_enforces_the_configured_delay() {
        let app = App::new(Settings::default()).unwrap();
        app.defer_next_fetch(Duration::from_millis(30)).await;
        let started = Instant::now();
        let generation = app.fetch_generation.load(Ordering::SeqCst);
        assert!(app.wait_for_fetch_slot(generation).await);
        assert!(started.elapsed() >= Duration::from_millis(20));
    }

    #[tokio::test]
    async fn generation_change_interrupts_a_waiting_fetch_slot() {
        let app = Arc::new(App::new(Settings::default()).unwrap());
        app.defer_next_fetch(Duration::from_secs(5)).await;
        let generation = app.fetch_generation.load(Ordering::SeqCst);
        let waiter = {
            let app = app.clone();
            tokio::spawn(async move { app.wait_for_fetch_slot(generation).await })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        app.fetch_generation.fetch_add(2, Ordering::SeqCst);
        app.fetch_change_notify.notify_waiters();
        assert!(!waiter.await.unwrap());
    }

    #[test]
    fn same_network_keeps_session_placeholder_until_bound() {
        let settings = Settings {
            outbound_proxy: "socks5://xmtt1126849-region-DE-sid-{session}-t-120:pass@us.arxlabs.io:3010".into(),
            outbound_mode: OutboundMode::Manual,
            network_route_policy: NetworkRoutePolicy::SameNetwork,
            ..Settings::default()
        };
        let template = resolved_business_proxy(&settings, &WarpRuntime::default(), &MihomoRuntime::default());
        assert!(fetch::has_session_placeholder(&template));
        assert!(fetch::apply_bound_session(&template, None).is_err());
        assert!(fetch::apply_bound_session(&template, Some("1Z5jzVPs"))
            .unwrap()
            .contains("-sid-1Z5jzVPs-t-120"));
        assert!(business_http_client(&template, None).is_ok());
        assert!(business_http_client(&template, Some("1Z5jzVPs")).is_ok());
    }

    fn test_login_jwt(account: &str, email: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({ "chatgpt_account_id": account, "email": email }).to_string(),
        );
        format!("{header}.{payload}.sig")
    }

    #[tokio::test]
    async fn fetch_once_does_not_refresh_login_before_probing_state() {
        if std::env::var_os("CSK_FETCH_REFRESH_TEST_CHILD").is_none() {
            let dir = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proxy::tests::fetch_once_does_not_refresh_login_before_probing_state",
                    "--nocapture",
                ])
                .env("CSK_FETCH_REFRESH_TEST_CHILD", "1")
                .env("HOME", dir.path())
                .env("USERPROFILE", dir.path())
                .env("APPDATA", dir.path())
                .env("LOCALAPPDATA", dir.path())
                .env_remove("HTTP_PROXY")
                .env_remove("HTTPS_PROXY")
                .env_remove("ALL_PROXY")
                .env_remove("http_proxy")
                .env_remove("https_proxy")
                .env_remove("all_proxy")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let home = tempfile::tempdir().unwrap();
        let old_access = test_login_jwt("probe-acct", "old@example.com");
        let new_access = test_login_jwt("probe-acct", "new@example.com");
        std::fs::write(
            login::kit_auth_path(home.path()),
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": old_access,
                    "access_token": old_access,
                    "refresh_token": "probe-refresh",
                    "account_id": "probe-acct"
                }
            })
            .to_string(),
        )
        .unwrap();

        let (oauth_send, mut oauth_requests) = tokio::sync::mpsc::channel::<serde_json::Value>(2);
        let oauth_access = new_access.clone();
        let oauth_app = axum::Router::new().route(
            "/oauth/token",
            axum::routing::post(move |axum::Json(payload): axum::Json<serde_json::Value>| {
                let oauth_send = oauth_send.clone();
                let oauth_access = oauth_access.clone();
                async move {
                    oauth_send.send(payload).await.unwrap();
                    axum::Json(serde_json::json!({
                        "access_token": oauth_access,
                        "refresh_token": "rotated-probe-refresh"
                    }))
                }
            }),
        );
        let oauth_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let oauth_url = format!("http://{}/oauth/token", oauth_listener.local_addr().unwrap());
        let oauth_server = tokio::spawn(async move {
            axum::serve(oauth_listener, oauth_app).await.unwrap();
        });

        let fresh = ticket_for_len(turn_state::QUALITY_TOKEN_LEN);
        let (probe_send, mut probe_headers) = tokio::sync::mpsc::unbounded_channel::<HeaderMap>();
        let reply_token = fresh.clone();
        let probe_app = axum::Router::new().fallback(move |headers: HeaderMap| {
            let probe_send = probe_send.clone();
            let token = reply_token.clone();
            async move {
                probe_send.send(headers).unwrap();
                Response::builder()
                    .header(turn_state::HEADER_NAME, token)
                    .body(Body::from(
                        "data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-6-astra\"}}\n\n",
                    ))
                    .unwrap()
            }
        });
        let probe_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", probe_listener.local_addr().unwrap());
        let probe_server = tokio::spawn(async move {
            axum::serve(probe_listener, probe_app).await.unwrap();
        });

        let app = App::new(Settings {
            upstream: endpoint.clone(),
            outbound_proxy: endpoint,
            outbound_mode: OutboundMode::Manual,
            codex_home: home.path().display().to_string(),
            models: vec!["gpt-6-astra".into()],
            ..Settings::default()
        })
        .unwrap();
        app.set_oauth_token_url(oauth_url);
        app.sync_logged_in_account().await;
        let token = app.fetch_once("gpt-6-astra").await.unwrap();
        assert_eq!(token, fresh);

        assert!(oauth_requests.try_recv().is_err());
        let headers = probe_headers.recv().await.unwrap();
        assert_eq!(
            headers[header::AUTHORIZATION],
            format!("Bearer {old_access}")
        );

        let kit: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(login::kit_auth_path(home.path())).unwrap(),
        )
        .unwrap();
        assert_eq!(kit["tokens"]["access_token"], old_access);
        assert_eq!(kit["tokens"]["refresh_token"], "probe-refresh");

        let cached = app.fetch_once_inner("gpt-6-astra", true).await.unwrap();
        assert_eq!(cached, fresh);
        assert!(oauth_requests.try_recv().is_err());

        oauth_server.abort();
        probe_server.abort();
    }

    #[tokio::test]
    async fn injected_ticket_replays_routing_cookies_and_drops_session() {
        if std::env::var_os("CSK_COOKIE_TEST_CHILD").is_none() {
            let dir = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proxy::tests::injected_ticket_replays_routing_cookies_and_drops_session",
                    "--nocapture",
                ])
                .env("CSK_COOKIE_TEST_CHILD", "1")
                .env("HOME", dir.path())
                .env("USERPROFILE", dir.path())
                .env("APPDATA", dir.path())
                .env("LOCALAPPDATA", dir.path())
                .env_remove("HTTP_PROXY")
                .env_remove("HTTPS_PROXY")
                .env_remove("ALL_PROXY")
                .env_remove("http_proxy")
                .env_remove("https_proxy")
                .env_remove("all_proxy")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            login::kit_auth_path(home.path()),
            serde_json::json!({
                "auth_mode":"chatgpt",
                "tokens":{
                    "access_token":"cookie-access",
                    "refresh_token":"cookie-refresh",
                    "account_id":"account-a"
                }
            })
            .to_string(),
        )
        .unwrap();
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel::<HeaderMap>();
        let upstream = axum::Router::new().fallback(move |headers: HeaderMap| {
            let sent = sent.clone();
            async move {
                sent.send(headers).unwrap();
                "ok"
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Arc::new(
            App::new(Settings {
                token_reuse_policy: TokenReusePolicy::PerModel,
                network_route_policy: NetworkRoutePolicy::Separate,
                upstream: format!("http://{}", listener.local_addr().unwrap()),
                codex_home: home.path().display().to_string(),
                ..Settings::default()
            })
            .unwrap(),
        );
        app.sync_logged_in_account().await;
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let fresh = ticket_for_len(turn_state::QUALITY_TOKEN_LEN);
        assert!(app.turn_state.lock().await.capture_with_session(
            "policy-model",
            &fresh,
            "test",
            None,
            None,
            &[RoutingCookie {
                name: "__oailb".into(),
                value: "route1".into(),
                expires_unix: None,
            }],
        ));
        let response = proxy_http(
            app.clone(),
            Request::builder()
                .method("POST")
                .uri("/responses")
                .header("chatgpt-account-id", "account-a")
                .header(header::COOKIE, "session=old; chatgpt_session=nope")
                .body(Body::from(r#"{"model":"policy-model"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let headers = received.recv().await.unwrap();
        let cookie = headers[header::COOKIE].to_str().unwrap();
        assert!(cookie.contains("__oailb=route1"));
        assert!(!cookie.contains("session"));
        assert_eq!(headers[turn_state::HEADER_NAME], fresh);
        server.abort();
    }

    #[test]
    fn idle_timeout_is_longer_before_the_first_chunk() {
        assert_eq!(business_stream_idle_timeout(0), Duration::from_secs(180));
        assert_eq!(business_stream_idle_timeout(2), Duration::from_secs(90));
    }

    #[tokio::test]
    async fn idle_sse_stream_is_cut_so_the_client_can_retry() {
        if std::env::var_os("CSK_IDLE_STREAM_TEST_CHILD").is_none() {
            let dir = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proxy::tests::idle_sse_stream_is_cut_so_the_client_can_retry",
                    "--nocapture",
                ])
                .env("CSK_IDLE_STREAM_TEST_CHILD", "1")
                .env("CSK_SSE_IDLE_MS", "80")
                .env("HOME", dir.path())
                .env("USERPROFILE", dir.path())
                .env("APPDATA", dir.path())
                .env("LOCALAPPDATA", dir.path())
                .env_remove("HTTP_PROXY")
                .env_remove("HTTPS_PROXY")
                .env_remove("ALL_PROXY")
                .env_remove("http_proxy")
                .env_remove("https_proxy")
                .env_remove("all_proxy")
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }

        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            login::kit_auth_path(home.path()),
            serde_json::json!({
                "auth_mode":"chatgpt",
                "tokens":{
                    "access_token":"idle-access",
                    "refresh_token":"idle-refresh",
                    "account_id":"account-a"
                }
            })
            .to_string(),
        )
        .unwrap();
        let upstream = axum::Router::new().fallback(|| async {
            (
                [(header::CONTENT_TYPE, "text/event-stream")],
                Body::from_stream(
                    futures_util::stream::once(async {
                        Ok::<_, std::io::Error>(Bytes::from_static(
                            b"data: {\"type\":\"response.created\"}\n\n",
                        ))
                    })
                    .chain(futures_util::stream::pending()),
                ),
            )
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Arc::new(
            App::new(Settings {
                network_route_policy: NetworkRoutePolicy::Separate,
                upstream: format!("http://{}", listener.local_addr().unwrap()),
                codex_home: home.path().display().to_string(),
                ..Settings::default()
            })
            .unwrap(),
        );
        app.sync_logged_in_account().await;
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let response = proxy_http(
            app.clone(),
            Request::builder()
                .method("POST")
                .uri("/responses")
                .header("accept", "text/event-stream")
                .header("chatgpt-account-id", "account-a")
                .body(Body::from(r#"{"model":"policy-model"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("response.incomplete"));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let entry = app.logs.lock().await.back().cloned();
                if entry.as_ref().is_some_and(|entry| !entry.in_progress) {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let entry = app.logs.lock().await.back().unwrap().snapshot();
        assert_eq!(entry.error_kind.as_deref(), Some("stream_idle"));
        assert_eq!(entry.stream_state, "error");
        server.abort();
    }
}

#[cfg(test)]
#[path = "token_reuse_tests.rs"]
mod token_reuse_tests;
