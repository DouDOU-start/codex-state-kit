use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode, Uri, Version};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde::Serialize;
use serde_json::json;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use url::Url;

use crate::accounts::{self, AccountEnvironment, NetworkProfile};
use crate::attach::{self, is_attached};
use crate::billing::{BillingStore, RequestStart, UsageOutcome, UsageState};
use crate::diag;
use crate::downgrade;
use crate::identity::{self, VmIdentity};
use crate::login;
#[cfg(test)]
use crate::logs::ObservedStream;
use crate::logs::{self, LogEntry, NetworkLogDetails, StreamLifecycle};
use crate::mihomo::{MihomoRuntime, MihomoStatus};
use crate::outbound;
use crate::settings::{save_settings, OutboundMode, Settings, SettingsPatch};
use crate::traffic::{AccountTraffic, RequestActivity, TrafficTracker};
use crate::ws_bridge;
use crate::ws_upstream::{WsDial, WsUpstreamPool};

/// 已有数据块后，上游再静默这么久就切断，让 Codex 能报错重试。
const SSE_IDLE_AFTER_CHUNK: Duration = Duration::from_secs(90);
/// 响应头已到但还没有任何正文时，多等一会儿，避免误杀长思考。
const SSE_IDLE_BEFORE_CHUNK: Duration = Duration::from_secs(180);
const SSE_IDLE_TIMEOUT_EVENT: &[u8] = b"data: {\"type\":\"response.incomplete\"}\n\n";
const UPSTREAM_TRANSPORT_HEADER: &str = "x-csk-upstream-transport";
/// How often the warm upstream WebSocket is pinged and checked.
const WS_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(25);

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
    pub outbound_mode: OutboundMode,
    pub mihomo_subscription: String,
    pub mihomo_node: String,
    pub mihomo: MihomoStatus,
    pub logs: Vec<LogEntry>,
    pub account_traffic: AccountTraffic,
    pub current_account_id: Option<String>,
    pub current_account_email: Option<String>,
    pub forced_model: String,
    pub diag_log_path: String,
    pub vm_identity: identity::VmIdentityView,
    pub chain_system_proxy: bool,
    /// Detected OS system proxy and the relay's last error.
    pub system_proxy: crate::system_proxy::SystemProxyView,
    /// Latest downgraded request since Kit started.
    pub last_downgrade: Option<crate::billing::DowngradeEvent>,
}

pub struct App {
    pub mihomo: MihomoRuntime,
    pub settings: Mutex<Settings>,
    pub logs: Mutex<VecDeque<LogEntry>>,
    /// Durable usage/cost accounting. The network log remains bounded and in-memory.
    pub billing: Arc<BillingStore>,
    traffic: TrafficTracker,
    pub proxy_ok: AtomicBool,
    pub login_http: reqwest::Client,
    leftover_restored: AtomicBool,
    proxy_error: Mutex<Option<String>>,
    /// Serializes identity and settings transitions.
    transition: Mutex<()>,
    http: Mutex<PooledUpstream>,
    pub sidecar_wake: Notify,
    vm_identity: Mutex<VmIdentity>,
    ws_upstream: WsUpstreamPool,
}

impl App {
    pub fn new(settings: Settings) -> Result<Self> {
        Self::with_mihomo(settings, MihomoRuntime::default())
    }

    pub fn with_mihomo(settings: Settings, mihomo: MihomoRuntime) -> Result<Self> {
        if !cfg!(test) {
            remove_legacy_ticket_cache();
        }
        crate::system_proxy::set_enabled(settings.chain_system_proxy);
        let business_proxy = resolved_proxy(&settings, &mihomo);
        let http = pooled_upstream(business_proxy_key(&business_proxy, None)?)?;
        let billing_path = crate::home_dir().join(if cfg!(debug_assertions) {
            ".codex-state-kit-dev-billing.sqlite3"
        } else {
            ".codex-state-kit-billing.sqlite3"
        });
        let billing = if cfg!(test) {
            BillingStore::open_in_memory()?
        } else {
            let pricing = crate::pricing::PriceBook::load(crate::home_dir().join(
                if cfg!(debug_assertions) {
                    ".codex-state-kit-dev-pricing.json"
                } else {
                    ".codex-state-kit-pricing.json"
                },
            ));
            BillingStore::open(billing_path, Arc::new(pricing))?
        };
        Ok(Self {
            mihomo,
            settings: Mutex::new(settings),
            logs: Mutex::new(VecDeque::with_capacity(80)),
            billing: Arc::new(billing),
            traffic: TrafficTracker::default(),
            proxy_ok: AtomicBool::new(false),
            login_http: crate::login::http_client()?,
            leftover_restored: AtomicBool::new(false),
            proxy_error: Mutex::new(None),
            transition: Mutex::new(()),
            http: Mutex::new(http),
            sidecar_wake: Notify::new(),
            vm_identity: Mutex::new(if cfg!(test) {
                VmIdentity::ephemeral()
            } else {
                VmIdentity::load_or_create()
            }),
            ws_upstream: WsUpstreamPool::new(),
        })
    }

    /// The credentials for the next upstream request, and whether Kit must
    /// override the client's own auth headers with them.
    async fn sync_request_identity(
        &self,
        home: &Path,
    ) -> Option<(login::ChatGptCredentials, bool)> {
        let _transition = self.transition.lock().await;
        login::request_credentials(home).ok()
    }

    async fn sync_logged_in_account(&self) {
        let home = self.settings.lock().await.codex_home.clone();
        let _ = self.sync_request_identity(Path::new(&home)).await;
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
            outbound_mode: settings.outbound_mode,
            mihomo_subscription: settings.mihomo_subscription.clone(),
            mihomo_node: settings.mihomo_node.clone(),
            mihomo: self.mihomo.status(),
            logs,
            account_traffic,
            current_account_id: account,
            current_account_email: login_status.email,
            forced_model: settings.forced_model,
            diag_log_path: diag::path().display().to_string(),
            vm_identity: self.vm_identity.lock().await.view(),
            chain_system_proxy: settings.chain_system_proxy,
            system_proxy: tokio::task::spawn_blocking(crate::system_proxy::view)
                .await
                .unwrap_or_else(|_| crate::system_proxy::view()),
            last_downgrade: self.billing.last_downgrade(),
        }
    }

    /// The outbound line for ChatGPT login and token import: the same exit
    /// (and `{session}`) business requests use, so a new account signs in
    /// from the line it is then bound to. Empty when no line is configured.
    ///
    /// `account_id` names a saved account being re-authorized: it signs in
    /// on its own manual proxy. Subscription nodes are picked in the core
    /// that carries live traffic, so an account on a subscription line signs
    /// in on the live line instead.
    pub async fn login_proxy(&self, account_id: Option<&str>) -> String {
        let settings = self.settings.lock().await.clone();
        let own = account_id
            .and_then(|id| accounts::sign_in_network(Path::new(&settings.codex_home), id).ok()?)
            .filter(|network| network.outbound_mode == OutboundMode::Manual);
        let template = match own {
            Some(network) => network.outbound_proxy,
            None => resolved_proxy(&settings, &self.mihomo),
        };
        business_proxy_key(&template, None).unwrap_or_default()
    }

    async fn refresh_business_http(&self) -> Result<()> {
        let settings = self.settings.lock().await.clone();
        let proxy = resolved_proxy(&settings, &self.mihomo);
        *self.http.lock().await = pooled_upstream(business_proxy_key(&proxy, None)?)?;
        Ok(())
    }

    async fn business_client(&self, key: &str) -> Result<reqwest::Client> {
        let mut slot = self.http.lock().await;
        if slot.key != key {
            *slot = pooled_upstream(key.to_string())?;
        }
        Ok(slot.client.clone())
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
        }
        let entry = LogEntry::new(method, path, status, started, details);
        let mut logs = self.logs.lock().await;
        logs::push(&mut logs, entry);
    }

    /// Opens the usage record for a WebSocket turn, attributed like HTTP
    /// turns: to the credentials Kit sends upstream.
    fn begin_ws_billing(
        &self,
        started: Instant,
        account: &BillingAccount,
        requested_model: Option<&str>,
        sent_model: Option<&str>,
        service_tier: Option<String>,
    ) -> Option<BillingRequest> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now()
            .checked_sub_signed(chrono::Duration::from_std(started.elapsed()).unwrap_or_default())
            .unwrap_or_else(chrono::Utc::now)
            .to_rfc3339();
        match self.billing.begin_request(RequestStart {
            request_id: request_id.clone(),
            provider: "chatgpt".into(),
            account_id: account.id.clone(),
            email: account.email.clone(),
            source: "business".into(),
            started_at,
            requested_model: requested_model.or(sent_model).map(str::to_owned),
            sent_model: sent_model.map(str::to_owned),
            service_tier,
        }) {
            Ok(_) => Some(BillingRequest::new(self.billing.clone(), request_id)),
            Err(error) => {
                eprintln!("[billing] begin websocket record failed: {error:#}");
                None
            }
        }
    }
}

#[derive(Clone)]
pub struct ProxyHandle {
    app: Arc<App>,
    stop: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    task: Arc<Mutex<Option<JoinHandle<()>>>>,
    settings_change: Arc<Mutex<()>>,
    managed_routes: Arc<std::sync::Mutex<Option<attach::ManagedRoutes>>>,
    attach_error: Arc<std::sync::Mutex<Option<String>>>,
    /// Serialises binding the live environment to accounts.
    account_env: Arc<Mutex<()>>,
}

impl ProxyHandle {
    pub fn new(app: Arc<App>) -> Self {
        Self {
            app,
            stop: Arc::new(Mutex::new(None)),
            task: Arc::new(Mutex::new(None)),
            settings_change: Arc::new(Mutex::new(())),
            managed_routes: Arc::new(std::sync::Mutex::new(None)),
            attach_error: Arc::new(std::sync::Mutex::new(None)),
            account_env: Arc::new(Mutex::new(())),
        }
    }

    pub fn app(&self) -> Arc<App> {
        self.app.clone()
    }

    pub fn enable_auto_attach(&self) {
        *self.managed_routes.lock().expect("managed routes") =
            Some(attach::ManagedRoutes::new(attach::backup_path()));
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
        *self.attach_error.lock().expect("attach error") = result
            .as_ref()
            .err()
            .map(|err| format!("自动接入失败：{err:#}"));
        result
    }

    pub async fn update_vm_identity(&self, profile: identity::VmProfile) -> Result<Status> {
        {
            let mut identity = self.app.vm_identity.lock().await;
            identity.apply_profile(profile);
            identity.save()?;
        }
        self.app.ws_upstream.invalidate().await;
        self.remember_account_environment().await;
        Ok(self.managed_status().await)
    }

    pub async fn regenerate_vm_installation_id(&self) -> Result<Status> {
        {
            let mut identity = self.app.vm_identity.lock().await;
            identity.regenerate_installation_id();
            identity.save()?;
        }
        self.app.ws_upstream.invalidate().await;
        self.remember_account_environment().await;
        Ok(self.managed_status().await)
    }

    pub async fn detect_vm_cli_version(&self) -> Result<Status> {
        let version = tokio::task::spawn_blocking(identity::detect_local_cli_version)
            .await
            .context("检测 Codex CLI 版本")?
            .context("没有检测到本机 codex --version")?;
        {
            let mut identity = self.app.vm_identity.lock().await;
            identity.cli_version = version;
            identity.save()?;
        }
        self.app.ws_upstream.invalidate().await;
        self.remember_account_environment().await;
        Ok(self.managed_status().await)
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
        Ok(())
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
        if let Some(tx) = self.stop.lock().await.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.task.lock().await.take() {
            let _ = handle.await;
        }
        self.app.proxy_ok.store(false, Ordering::Relaxed);
    }

    pub async fn apply_settings(&self, patch: SettingsPatch) -> Result<Status> {
        let status = self.apply_settings_to(patch.into_settings()?).await?;
        // Outbound edits belong to the account that is currently live.
        self.remember_account_environment().await;
        Ok(status)
    }

    /// The environment Kit is using right now: virtual device and outbound line.
    async fn current_environment(&self) -> AccountEnvironment {
        let settings = self.app.settings.lock().await.clone();
        let vm = self.app.vm_identity.lock().await.clone();
        let mihomo = self.app.mihomo.status();
        let mihomo_selections =
            if settings.outbound_mode == OutboundMode::Mihomo && mihomo.phase == "connected" {
                mihomo
                    .groups
                    .iter()
                    .filter(|group| {
                        matches!(
                            group.group_type.to_ascii_lowercase().as_str(),
                            "select" | "selector"
                        )
                    })
                    .filter_map(|group| group.now.clone().map(|node| (group.name.clone(), node)))
                    .collect()
            } else {
                Default::default()
            };
        AccountEnvironment {
            vm,
            network: NetworkProfile {
                outbound_mode: settings.outbound_mode,
                outbound_proxy: settings.outbound_proxy,
                mihomo_subscription: settings.mihomo_subscription,
                mihomo_node: settings.mihomo_node,
                mihomo_selections,
            },
        }
    }

    /// Makes an account's saved environment live.
    async fn apply_environment(&self, target: &AccountEnvironment) -> Result<()> {
        {
            let mut live = self.app.vm_identity.lock().await;
            let mut next = target.vm.clone().with_runtime_ids();
            // The CLI version follows the codex installed here, not the account.
            next.cli_version = live.cli_version.clone();
            next.save()?;
            *live = next;
        }
        self.app.ws_upstream.invalidate().await;
        let mut next = self.app.settings.lock().await.clone();
        let network = &target.network;
        let network_changed = next.outbound_mode != network.outbound_mode
            || next.outbound_proxy != network.outbound_proxy
            || next.mihomo_subscription != network.mihomo_subscription
            || next.mihomo_node != network.mihomo_node;
        if network_changed {
            next.outbound_mode = network.outbound_mode;
            next.outbound_proxy = network.outbound_proxy.clone();
            next.mihomo_subscription = network.mihomo_subscription.clone();
            next.mihomo_node = network.mihomo_node.clone();
            self.apply_settings_to(next).await?;
        }
        if network.outbound_mode == OutboundMode::Mihomo && !network.mihomo_selections.is_empty() {
            let app = self.app.clone();
            let selections = network.mihomo_selections.clone();
            tokio::spawn(async move { restore_mihomo_selections(app, selections).await });
        }
        Ok(())
    }

    /// Binds the live environment to the live account: saves it for the
    /// account that owned it and, when the live account changed (switch or
    /// new login), makes that account's own environment live. A newly seen
    /// account gets a new virtual device and keeps the current line.
    pub async fn sync_account_environment(&self) -> Result<()> {
        let _guard = self.account_env.lock().await;
        let home = std::path::PathBuf::from(self.app.settings.lock().await.codex_home.clone());
        let current = self.current_environment().await;
        let Some(plan) = accounts::plan_environment(&home, &current, |env| AccountEnvironment {
            vm: env.vm.renewed(),
            network: env.network.clone(),
        })?
        else {
            return Ok(());
        };
        let live = match plan.target {
            Some(target) if !target.same_as(&current) => {
                self.apply_environment(&target).await?;
                target
            }
            Some(target) => target,
            None => current,
        };
        accounts::commit_environment(&home, &plan.account_id, &live)
    }

    async fn remember_account_environment(&self) {
        if let Err(err) = self.sync_account_environment().await {
            eprintln!("[accounts] 绑定账号环境失败: {err:#}");
        }
    }

    /// Switches the live account and its environment together.
    pub async fn switch_account(
        &self,
        home: &Path,
        account_id: &str,
    ) -> Result<login::LoginStatus> {
        let status = accounts::switch(home, account_id)?;
        self.sync_account_environment().await?;
        Ok(status)
    }

    async fn apply_settings_to(&self, next: Settings) -> Result<Status> {
        let _change = self.settings_change.lock().await;
        let old = self.app.settings.lock().await.clone();
        let next_business = resolved_proxy(&next, &self.app.mihomo);
        let next_http = if resolved_proxy(&old, &self.app.mihomo) != next_business {
            Some(pooled_upstream(business_proxy_key(&next_business, None)?)?)
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
            || old.upstream != next.upstream
            || old.codex_home != next.codex_home
            || old.mihomo_subscription != next.mihomo_subscription
            || old.mihomo_node != next.mihomo_node;
        // A route change must not interleave with a request picking its identity.
        let transition = if route_changed {
            Some(self.app.transition.lock().await)
        } else {
            None
        };
        if route_changed {
            self.app.ws_upstream.invalidate().await;
            if next.outbound_mode != OutboundMode::Mihomo {
                self.app.mihomo.stop().await;
            }
        }
        crate::system_proxy::set_enabled(next.chain_system_proxy);
        {
            let mut settings = self.app.settings.lock().await;
            *settings = next.clone();
            if let Some(http) = next_http {
                *self.app.http.lock().await = http;
            }
        }
        drop(transition);
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
            if self
                .managed_routes
                .lock()
                .expect("managed routes")
                .is_none()
            {
                attach::update_attached_base_url(&next)?;
            }
        }
        if next.outbound_mode == OutboundMode::Mihomo
            && (route_changed || self.app.mihomo.status().phase != "connected")
        {
            if let Err(err) = self.app.mihomo.start(&next).await {
                eprintln!("mihomo: {err:#}");
            }
            let _ = self.app.refresh_business_http().await;
        }
        self.app.sidecar_wake.notify_one();
        Ok(self.managed_status().await)
    }

    /// Keeps model prices in sync with sub2api's price repo: checks the
    /// published sha256 every 10 minutes and swaps in a newer catalog.
    /// Retries after a minute when the outbound line is not ready yet.
    pub async fn run_pricing_supervisor(&self) {
        loop {
            let wait = match self.sync_pricing().await {
                Ok(_) => crate::pricing::SYNC_INTERVAL,
                Err(err) => {
                    eprintln!("[pricing] 同步模型价格失败: {err:#}");
                    Duration::from_secs(60)
                }
            };
            tokio::time::sleep(wait).await;
        }
    }

    /// Fetches the price catalog through the configured outbound line.
    pub async fn sync_pricing(&self) -> Result<crate::pricing::CatalogInfo> {
        let settings = self.app.settings.lock().await.clone();
        let (proxy, _) =
            outbound::resolve_session_proxy(&resolved_proxy(&settings, &self.app.mihomo));
        let client = outbound::http_client(&proxy)?;
        let pricing = self.app.billing.pricing();
        if pricing.sync(&client).await? {
            let info = pricing.info();
            eprintln!(
                "[pricing] 已更新模型价格：{} 个模型，sha256 {}",
                info.model_count,
                &info.sha256[..info.sha256.len().min(12)]
            );
        }
        // Records that finished before their model had a price (the first
        // sync after startup included) are priced now.
        match self.app.billing.price_unpriced() {
            Ok(0) => {}
            Ok(count) => eprintln!("[pricing] 补算了 {count} 条未定价记录"),
            Err(err) => eprintln!("[pricing] 补算未定价记录失败: {err:#}"),
        }
        Ok(pricing.info())
    }

    /// Keeps an upstream WebSocket connected and healthy ahead of requests:
    /// warms one up, pings it, replaces it before it expires, and re-warms
    /// right after the account or outbound line changes.
    pub async fn run_ws_keeper(&self) {
        loop {
            if let Ok(dial) = warm_ws_dial(&self.app).await {
                self.app.ws_upstream.maintain(&dial).await;
            }
            tokio::select! {
                _ = tokio::time::sleep(WS_KEEPALIVE_INTERVAL) => {},
                _ = self.app.ws_upstream.changed() => {},
            }
        }
    }

    pub async fn run_sidecar_supervisor(&self) {
        loop {
            let mode = self.app.settings.lock().await.outbound_mode;
            let mut wait = Duration::from_secs(20);
            if mode == OutboundMode::Mihomo {
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
                _ = self.app.sidecar_wake.notified() => {},
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
            "manual" => {
                let raw = proxy
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or(settings.outbound_proxy);
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
        return proxy_ws(app, req).await;
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
    let mut billing_request = None;
    match forward_http_tracked(
        &app,
        req,
        &mut details,
        &mut activity,
        &mut billing_request,
        started,
    )
    .await
    {
        Ok(mut resp) => {
            details.response_header_ms = Some(started.elapsed().as_millis());
            details.response_content_encoding = Some(logs::safe_content_encoding(
                resp.headers()
                    .get(header::CONTENT_ENCODING)
                    .and_then(|value| value.to_str().ok()),
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
                if let Some(request) = billing_request.take() {
                    request.settle(UsageOutcome {
                        state: UsageState::MissingUsage,
                        finished_at: Some(chrono::Utc::now().to_rfc3339()),
                        http_status: Some(resp.status().as_u16()),
                        error_kind: None,
                        ..UsageOutcome::default()
                    });
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
            let is_sse = details.transport == "http_sse"
                || resp
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
            if let Some(transport) = resp
                .headers()
                .get(UPSTREAM_TRANSPORT_HEADER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
            {
                details.transport = transport;
            } else if is_sse {
                details.transport = "http_sse".into();
            }
            resp.headers_mut().remove(UPSTREAM_TRANSPORT_HEADER);
            let mut metrics = logs::ResponseBodyMetrics::new(
                resp.headers()
                    .get(header::CONTENT_ENCODING)
                    .map(|value| value.to_str().unwrap_or("unsupported"))
                    .unwrap_or_default(),
            );
            metrics.observe_headers(resp.headers());
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
                                "transport": details.transport,
                    }),
                );
            }
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
                billing: billing_request,
                billing_settled: false,
                lifecycle,
                diag: diag_req,
                diag_first_token: false,
                diag_finished: false,
                last_diag_chunks: 0,
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
                    let next = match tokio::time::timeout(
                        business_stream_idle_timeout(chunks),
                        stream.next(),
                    )
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
                        tracker.settle_billing();
                        tracker.activity.take();
                    }
                    // No read-ahead or buffering for forwarding. Only publish metric changes
                    // and the final result; response bytes are passed through unchanged.
                    if tracker.finished
                        || tracker.entry.first_token_ms != tracker.metrics.first_token_ms()
                        || tracker.entry.output_tokens != tracker.metrics.output_tokens()
                        || tracker.entry.upstream_response_model.as_deref()
                            != tracker.metrics.upstream_response_model()
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
            if let Some(request) = billing_request.take() {
                request.settle(UsageOutcome {
                    state: UsageState::Interrupted,
                    finished_at: Some(chrono::Utc::now().to_rfc3339()),
                    http_status: details.response_status,
                    error_kind: details.error_kind.clone(),
                    ..UsageOutcome::default()
                });
            }
            let status = details
                .response_status
                .and_then(|status| StatusCode::from_u16(status).ok())
                .unwrap_or(StatusCode::BAD_GATEWAY);
            app.record(method.as_str(), &path, status.as_u16(), started, details)
                .await;
            (status, err.to_string()).into_response()
        }
    }
}

/// The durable billing row is created before the upstream request is sent.
/// Keeping this small context with the response body makes account attribution
/// stable even when the login file changes while a stream is still running.
struct BillingRequest {
    store: Arc<BillingStore>,
    request_id: String,
    settled: AtomicBool,
}

impl BillingRequest {
    fn new(store: Arc<BillingStore>, request_id: String) -> Self {
        Self {
            store,
            request_id,
            settled: AtomicBool::new(false),
        }
    }

    fn settle(&self, outcome: UsageOutcome) {
        if self.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(error) = self.store.settle_request(&self.request_id, outcome) {
            self.settled.store(false, Ordering::Release);
            eprintln!("[billing] settle {} failed: {error:#}", self.request_id);
        }
    }
}

impl Drop for BillingRequest {
    fn drop(&mut self) {
        if self.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(error) = self.store.mark_interrupted(
            &self.request_id,
            None,
            Some("request_dropped"),
            Some(chrono::Utc::now().to_rfc3339()),
        ) {
            eprintln!(
                "[billing] mark dropped {} failed: {error:#}",
                self.request_id
            );
        }
    }
}

struct ResponseLogTracker {
    app: Arc<App>,
    entry: LogEntry,
    started: Instant,
    metrics: logs::ResponseBodyMetrics,
    finished: bool,
    remaining_bytes: Option<u64>,
    activity: Option<RequestActivity>,
    billing: Option<BillingRequest>,
    billing_settled: bool,
    lifecycle: Option<Arc<StreamLifecycle>>,
    diag: Option<diag::Request>,
    diag_first_token: bool,
    diag_finished: bool,
    last_diag_chunks: u64,
}

impl ResponseLogTracker {
    fn refresh(&mut self) {
        self.entry.ms = self.started.elapsed().as_millis();
        self.entry.first_token_ms = self.metrics.first_token_ms();
        self.entry.output_tokens = self.metrics.output_tokens();
        self.entry.upstream_response_model =
            self.metrics.upstream_response_model().map(str::to_owned);
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

    fn settle_billing(&mut self) {
        if self.billing_settled {
            return;
        }
        let Some(request) = self.billing.as_ref() else {
            self.billing_settled = true;
            return;
        };
        let usage_complete = self.metrics.usage_seen()
            && self.metrics.input_tokens().is_some()
            && self.metrics.output_tokens().is_some();
        let state = if self.entry.error_kind.is_some() || self.entry.status >= 400 {
            UsageState::Interrupted
        } else if usage_complete {
            UsageState::Measured
        } else {
            UsageState::MissingUsage
        };
        request.settle(UsageOutcome {
            state,
            finished_at: Some(chrono::Utc::now().to_rfc3339()),
            http_status: Some(self.entry.status),
            response_model: self.metrics.upstream_response_model().map(str::to_owned),
            usage: self.metrics.token_usage(),
            usage_source: self
                .metrics
                .usage_seen()
                .then(|| "provider_response".into()),
            error_kind: self.entry.error_kind.clone(),
            service_tier: self.metrics.service_tier().map(str::to_owned),
            first_token_ms: self.metrics.first_token_ms().map(|ms| ms as u64),
            transport: Some(self.entry.transport.clone()),
            downgrade_signals: self.metrics.downgrade_signals().clone(),
        });
        self.billing_settled = true;
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
        if chunks > 0 && chunks != self.last_diag_chunks && (chunks <= 8 || chunks % 20 == 0) {
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
        if !self.billing_settled {
            // A dropped body means the client disconnected before a reliable
            // terminal usage event. Preserve the row as interrupted.
            if let Some(request) = self.billing.as_ref() {
                request.settle(UsageOutcome {
                    state: UsageState::Interrupted,
                    finished_at: Some(chrono::Utc::now().to_rfc3339()),
                    http_status: Some(self.entry.status),
                    usage: self.metrics.token_usage(),
                    service_tier: self.metrics.service_tier().map(str::to_owned),
                    first_token_ms: self.metrics.first_token_ms().map(|ms| ms as u64),
                    transport: Some(self.entry.transport.clone()),
                    downgrade_signals: self.metrics.downgrade_signals().clone(),
                    usage_source: self
                        .metrics
                        .usage_seen()
                        .then(|| "provider_response".into()),
                    error_kind: self
                        .entry
                        .error_kind
                        .clone()
                        .or_else(|| Some("client_cancelled".into())),
                    ..UsageOutcome::default()
                });
            }
            self.billing_settled = true;
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

/// Re-selects an account's subscription nodes once the core is running.
async fn restore_mihomo_selections(
    app: Arc<App>,
    selections: std::collections::BTreeMap<String, String>,
) {
    for _ in 0..60 {
        if app.mihomo.status().phase == "connected" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let groups = app.mihomo.status().groups;
    let mut changed = false;
    for (group, node) in selections {
        let current = groups.iter().find(|item| item.name == group);
        let available = current.is_some_and(|item| item.all.iter().any(|n| n.name == node));
        if !available || current.and_then(|item| item.now.as_deref()) == Some(node.as_str()) {
            continue;
        }
        match app.mihomo.select_in_group(&group, &node).await {
            Ok(()) => changed = true,
            Err(err) => eprintln!("[accounts] 恢复订阅节点 {group} → {node} 失败: {err:#}"),
        }
    }
    if changed {
        app.ws_upstream.invalidate().await;
    }
}

/// Older versions cached turn-state tickets on disk; they are no longer used.
fn remove_legacy_ticket_cache() {
    let path = crate::home_dir().join(if cfg!(debug_assertions) {
        ".codex-state-kit-dev-token.json"
    } else {
        ".codex-state-kit-token.json"
    });
    match std::fs::remove_file(&path) {
        Ok(()) => println!("removed legacy ticket cache {}", path.display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => eprintln!("failed to remove {}: {err}", path.display()),
    }
}

fn resolved_proxy(settings: &Settings, mihomo: &MihomoRuntime) -> String {
    match settings.outbound_mode {
        OutboundMode::Mihomo => mihomo.proxy_url().unwrap_or_default(),
        OutboundMode::Manual => settings.outbound_proxy.clone(),
    }
}

fn business_network_details(settings: &Settings, upstream: &str, proxy: &str) -> NetworkLogDetails {
    let mut details = logs::network_details(upstream, proxy);
    if !proxy.trim().is_empty() {
        details.route_kind = match settings.outbound_mode {
            OutboundMode::Mihomo => logs::ROUTE_EMBEDDED_MIHOMO.into(),
            OutboundMode::Manual => logs::ROUTE_MANUAL_PROXY.into(),
        };
    }
    details
}

struct PooledUpstream {
    key: String,
    client: reqwest::Client,
}

fn business_proxy_key(template: &str, session: Option<&str>) -> Result<String> {
    match outbound::apply_bound_session(template, session) {
        Ok(proxy) => Ok(proxy),
        Err(_) if outbound::has_session_placeholder(template) => {
            Ok(outbound::replace_session_placeholder(template, "unbound0"))
        }
        Err(err) => Err(err),
    }
}

fn pooled_upstream(key: String) -> Result<PooledUpstream> {
    let client = upstream_http_client(&key)?;
    Ok(PooledUpstream { key, client })
}

#[cfg(test)]
fn business_http_client(template: &str, session: Option<&str>) -> Result<reqwest::Client> {
    Ok(pooled_upstream(business_proxy_key(template, session)?)?.client)
}

fn upstream_http_client(proxy: &str) -> Result<reqwest::Client> {
    let proxy = crate::settings::normalize_proxy(proxy, "上游转发代理")?;
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(8))
        .redirect(reqwest::redirect::Policy::limited(5))
        .pool_idle_timeout(Duration::from_secs(90))
        .pool_max_idle_per_host(4)
        // 业务转发要原样交回下游。关掉自动解压，避免吃掉 Codex 自己的 Content-Encoding。
        .no_gzip()
        .no_zstd();
    if !proxy.is_empty() {
        let proxy = reqwest::Proxy::all(outbound::dial_proxy_for_client(&proxy))
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
    let response = forward_http_tracked(app, req, details, &mut None, &mut None, started).await?;
    let Some(lifecycle) = details.stream_lifecycle.clone() else {
        return Ok(response);
    };
    let (parts, body) = response.into_parts();
    Ok(Response::from_parts(
        parts,
        Body::from_stream(ObservedStream::new(body.into_data_stream(), lifecycle)),
    ))
}

async fn forward_http_tracked(
    app: &App,
    req: Request<Body>,
    details: &mut NetworkLogDetails,
    activity: &mut Option<RequestActivity>,
    billing_request: &mut Option<BillingRequest>,
    started: Instant,
) -> Result<Response> {
    let (upstream, home, upstream_proxy, request_settings) = {
        let settings = app.settings.lock().await;
        let business_proxy = resolved_proxy(&settings, &app.mihomo);
        (
            settings.upstream.clone(),
            settings.codex_home.clone(),
            business_proxy,
            settings.clone(),
        )
    };
    let effective_proxy = outbound::outbound_proxy_for_client(&upstream_proxy);
    *details = business_network_details(&request_settings, &upstream, &effective_proxy);
    if request_settings.outbound_mode == OutboundMode::Mihomo && upstream_proxy.trim().is_empty() {
        anyhow::bail!(
            "{}",
            app.mihomo
                .proxy_url()
                .err()
                .map(|err| err.to_string())
                .unwrap_or_else(|| "订阅节点正在连接，请稍候。".into())
        );
    }
    let (mut parts, body) = req.into_parts();
    let target = join_upstream(&upstream, &parts.uri)?;
    let path = parts.uri.path();

    // 先读取 body，以便从中提取或改写 model 字段
    let mut bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .context("read body")?;
    details.body_bytes = bytes.len();

    let content_encoding = parts
        .headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok())
        .map(|value| value.to_string());
    details.content_encoding = logs::safe_content_encoding(content_encoding.as_deref());
    // The model the client asked for, before a forced model replaces it.
    let client_model = crate::body_model::extract_model_from_body(&bytes);
    if path.contains("/responses") {
        if let Some(forced) = request_settings.forced_model() {
            bytes = crate::body_model::rewrite_model_in_body(
                &bytes,
                content_encoding.as_deref(),
                forced,
            )
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
        "[proxy] {} {} body={}bytes encoding={}",
        parts.method,
        path,
        bytes.len(),
        details.content_encoding,
    ));

    let request_identity = app.sync_request_identity(Path::new(&home)).await;
    let billing_identity_matches =
        request_identity
            .as_ref()
            .is_some_and(|(creds, override_headers)| {
                *override_headers || !login::credentials_conflict_headers(&parts.headers, creds)
            });
    if let Some((creds, true)) = &request_identity {
        if !login::apply_chatgpt_credentials_headers(&mut parts.headers, creds) {
            anyhow::bail!("无法应用 ChatGPT 鉴权请求头");
        }
    }
    let effective_account = request_account(&parts.headers);
    if let Some((creds, _)) = request_identity
        .as_ref()
        .filter(|_| billing_identity_matches)
    {
        // The persisted identity is the credential selected by Kit, never a
        // free-form client header. This also keeps the log and billing keys in
        // sync when Kit has overridden the official Codex credentials.
        details.account_id = Some(logs::safe_text(&creds.account_id, 128));
        details.account_email = creds
            .email
            .as_deref()
            .map(|email| logs::safe_text(email, 254));
    } else {
        details.account_id = effective_account
            .as_deref()
            .map(|id| logs::safe_text(id, 128));
    }
    let request_model = crate::body_model::extract_model_from_body(&bytes);
    details.model = request_model
        .as_deref()
        .map(|model| logs::safe_text(model, 80))
        .filter(|model| !model.is_empty());
    details.diag = Some(diag::Request {
        id: diag::next_id(),
        flow: details.flow.clone(),
        model: details.model.clone(),
        route_kind: details.route_kind.clone(),
        proxy_session: None,
    });
    if let Some(req) = &details.diag {
        diag::emit(
            "request",
            Some(req),
            json!({
                "method": parts.method.as_str(),
                "path": path,
                "bodyBytes": details.body_bytes,
            }),
        );
    }
    parts.headers.remove(header::COOKIE);
    // Persist the account/request association before sending upstream. The
    // account comes from the credentials actually applied to this request, so
    // a late login switch cannot reassign an in-flight response.
    if parts.method == http::Method::POST && path.contains("/responses") && billing_identity_matches
    {
        let (account_id, email) = request_identity
            .as_ref()
            .map(|(creds, _)| (creds.account_id.clone(), creds.email.clone()))
            .expect("billing_identity_matches implies an identity");
        let request_id = uuid::Uuid::new_v4().to_string();
        let sent_model = request_model.clone();
        app.billing
            .begin_request(RequestStart {
                request_id: request_id.clone(),
                provider: "chatgpt".into(),
                account_id,
                email,
                source: "business".into(),
                started_at: chrono::Utc::now().to_rfc3339(),
                requested_model: client_model.or_else(|| request_model.clone()),
                sent_model,
                service_tier: crate::body_model::extract_str_field(
                    &bytes,
                    content_encoding.as_deref(),
                    "service_tier",
                ),
            })
            .context("begin durable billing record")?;
        *billing_request = Some(BillingRequest::new(app.billing.clone(), request_id));
    }
    let resolved_proxy = if outbound::has_session_placeholder(&upstream_proxy) {
        let (resolved, session) = outbound::resolve_session_proxy(&upstream_proxy);
        details.proxy_session = session.clone();
        details.proxy_endpoint = Some(logs::endpoint_origin(&resolved));
        resolved
    } else {
        upstream_proxy.clone()
    };
    if let Some(account) = parts
        .headers
        .get("chatgpt-account-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        *activity = Some(app.traffic.begin(account, Instant::now()));
    }
    let vm = app.vm_identity.lock().await.clone();
    match identity::rewrite_client_metadata_in_body(&bytes, content_encoding.as_deref(), &vm) {
        Ok(rewritten) => {
            bytes = rewritten.into();
            details.body_bytes = bytes.len();
        }
        Err(err) => {
            eprintln!("[identity] 请求体身份改写失败，保留原正文: {err}");
        }
    }
    // Business turns go over the upstream WebSocket, like the official client;
    // while the pool backs off after a failed handshake they use HTTP SSE.
    if ws_bridge::should_bridge_http(parts.method.as_str(), path, &target)
        && app.ws_upstream.available()
    {
        match forward_responses_over_ws(
            app,
            &target,
            &upstream_proxy,
            &parts.headers,
            &bytes,
            started,
            details,
        )
        .await
        {
            Ok(response) => return Ok(response),
            Err(err) => {
                eprintln!("[ws] 上游 WebSocket 失败: {err:#}，回退到 HTTP");
            }
        }
    }
    // WebSocket 链式续跑才认 previous_response_id；走 HTTP 时必须去掉，
    // 否则上游返回 "Invalid previous_response_id"。
    if parts.method == http::Method::POST && path.contains("/responses") {
        let stripped =
            crate::body_model::strip_previous_response_id(&bytes, content_encoding.as_deref());
        if stripped.as_slice() != bytes.as_ref() {
            bytes = stripped.into();
            details.body_bytes = bytes.len();
        }
    }
    let http = app.business_client(&resolved_proxy).await?;
    let mut builder = http
        .request(
            reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?,
            &target,
        )
        .body(bytes);
    for (name, value) in &parts.headers {
        if is_hop(name)
            || identity::is_vm_identity_header(name.as_str())
            || name.as_str().eq_ignore_ascii_case("x-codex-turn-state")
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder = builder
        .header("user-agent", vm.user_agent())
        .header("originator", &vm.originator)
        .header("version", &vm.cli_version)
        .header("x-codex-installation-id", &vm.installation_id)
        .header("x-codex-window-id", &vm.window_id)
        .header("session_id", &vm.session_id);
    if let Some(model) = request_model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty() && !model.chars().any(char::is_control))
    {
        builder = builder.header("x-codex-routing-hint", vm.routing_hint(model));
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

    eprintln!("[resp] {} {} → {}", parts.method, path, resp_status_u16);

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

async fn forward_responses_over_ws(
    app: &App,
    target: &str,
    proxy: &str,
    headers: &HeaderMap,
    bytes: &[u8],
    started: Instant,
    details: &mut NetworkLogDetails,
) -> Result<Response> {
    let mut frame =
        ws_bridge::http_body_to_ws_request(bytes).map_err(|err| anyhow::anyhow!(err))?;
    let identity = app.vm_identity.lock().await.clone();
    identity::rewrite_client_metadata_value(&mut frame, &identity);
    let model = frame
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    // The pool's sticky `{session}`, so turns reuse the same connections.
    let proxy = app.ws_upstream.resolve_proxy(proxy, None).await?;
    let dial = ws_dial(target, &proxy, headers, &identity, model)?;
    let (rx, handshake) = app.ws_upstream.open_turn(dial, frame).await?;
    let header_ms = started.elapsed().as_millis();
    details.transport = "http_to_ws".into();
    details.response_header_ms = Some(header_ms);
    details.http_version = Some("websocket".into());
    if let Ok(ws_url) = ws_bridge::upstream_to_ws_url(target) {
        details.final_origin = Some(logs::endpoint_origin(&ws_url));
    }
    details.stream_lifecycle = Some(Arc::new(StreamLifecycle::new(started, header_ms)));
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Some(Ok(text)) => {
                let line = Bytes::from(ws_bridge::ws_event_to_sse_line(&text));
                Some((Ok::<Bytes, std::io::Error>(line), rx))
            }
            Some(Err(err)) => {
                let line = Bytes::from(format!(
                    "data: {}\n\n",
                    serde_json::json!({"type": "response.incomplete", "error": err})
                ));
                Some((Ok(line), rx))
            }
            None => None,
        }
    });
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(UPSTREAM_TRANSPORT_HEADER, "http_to_ws");
    // Codex and the downgrade check both read these from the response head,
    // as they would on a plain HTTP stream.
    for name in downgrade::WATCHED_HEADERS {
        for value in handshake.get_all(name) {
            response = response.header(name, value.clone());
        }
    }
    response
        .body(Body::from_stream(stream))
        .context("构造 WebSocket SSE 响应")
}

fn ws_dial(
    target: &str,
    proxy: &str,
    headers: &HeaderMap,
    identity: &VmIdentity,
    model: &str,
) -> Result<WsDial> {
    let url = ws_bridge::upstream_to_ws_url(target).map_err(|err| anyhow::anyhow!(err))?;
    let mut extra_headers = vec![
        ("user-agent".into(), identity.user_agent()),
        ("originator".into(), identity.originator.clone()),
        ("version".into(), identity.cli_version.clone()),
        (
            "x-codex-installation-id".into(),
            identity.installation_id.clone(),
        ),
        ("x-codex-window-id".into(), identity.window_id.clone()),
        ("session_id".into(), identity.session_id.clone()),
        ("thread-id".into(), identity.thread_id.clone()),
        ("x-client-request-id".into(), identity.thread_id.clone()),
    ];
    let model = model.trim();
    if !model.is_empty() && !model.chars().any(char::is_control) {
        extra_headers.push(("x-codex-routing-hint".into(), identity.routing_hint(model)));
    }
    Ok(WsDial {
        url,
        proxy: outbound::dial_proxy_for_client(proxy),
        authorization: header_string(headers, "authorization"),
        account_id: header_string(headers, "chatgpt-account-id"),
        extra_headers,
    })
}

fn header_string(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string()
}

async fn proxy_ws(app: Arc<App>, req: Request<Body>) -> Response {
    let headers = req.headers().clone();
    let (mut parts, _body) = req.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade.on_upgrade(move |socket| async move {
            if let Err(err) = client_ws_session(app, headers, socket).await {
                eprintln!("[ws] 客户端会话结束: {err:#}");
            }
        }),
        Err(rejection) => rejection.into_response(),
    }
}

async fn client_ws_session(
    app: Arc<App>,
    client_headers: HeaderMap,
    mut socket: WebSocket,
) -> Result<()> {
    loop {
        let message = match socket.recv().await {
            Some(Ok(message)) => message,
            Some(Err(err)) => return Err(err).context("读取客户端 WebSocket"),
            None => return Ok(()),
        };
        let text = match message {
            WsMessage::Text(text) => text.to_string(),
            WsMessage::Close(_) => return Ok(()),
            WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Binary(_) => continue,
        };
        let started = Instant::now();
        let (frame, client_model) = prepare_client_ws_frame(&app, &text).await?;
        let model = frame
            .get("model")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let account = ws_billing_account(&app, &client_headers).await;
        let billing = account.as_ref().and_then(|account| {
            app.begin_ws_billing(
                started,
                account,
                client_model.as_deref(),
                Some(model.as_str()).filter(|model| !model.is_empty()),
                frame
                    .get("service_tier")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
            )
        });
        let dial = match current_ws_dial(&app, &client_headers, &model).await {
            Ok(dial) => dial,
            Err(err) => {
                let mut metrics = logs::ResponseBodyMetrics::new("");
                finish_client_ws_turn(
                    &app,
                    account.as_ref(),
                    &model,
                    started,
                    billing,
                    &mut metrics,
                    true,
                )
                .await;
                let fail = serde_json::json!({"type":"error","error":{"message": err.to_string()}});
                let _ = socket.send(WsMessage::text(fail.to_string())).await;
                continue;
            }
        };
        let (mut rx, handshake) = match app.ws_upstream.open_turn(dial, frame).await {
            Ok(opened) => opened,
            Err(err) => {
                let mut metrics = logs::ResponseBodyMetrics::new("");
                finish_client_ws_turn(
                    &app,
                    account.as_ref(),
                    &model,
                    started,
                    billing,
                    &mut metrics,
                    true,
                )
                .await;
                let fail = serde_json::json!({"type":"error","error":{"message": err.to_string()}});
                let _ = socket.send(WsMessage::text(fail.to_string())).await;
                continue;
            }
        };
        let mut metrics = logs::ResponseBodyMetrics::new("");
        metrics.observe_headers(&handshake);
        let mut failed = false;
        while let Some(event) = rx.recv().await {
            match event {
                Ok(json) => {
                    let line = ws_bridge::ws_event_to_sse_line(&json);
                    metrics.observe(line.as_bytes(), started.elapsed().as_millis(), true);
                    if socket.send(WsMessage::text(json)).await.is_err() {
                        failed = true;
                        break;
                    }
                }
                Err(_) => {
                    failed = true;
                    let fail = serde_json::json!({"type":"response.incomplete"});
                    let _ = socket.send(WsMessage::text(fail.to_string())).await;
                    break;
                }
            }
        }
        metrics.finish(started.elapsed().as_millis());
        finish_client_ws_turn(
            &app,
            account.as_ref(),
            &model,
            started,
            billing,
            &mut metrics,
            failed,
        )
        .await;
    }
}

/// The frame to send upstream, and the model the client asked for before a
/// forced model replaced it.
async fn prepare_client_ws_frame(
    app: &App,
    text: &str,
) -> Result<(serde_json::Value, Option<String>)> {
    let mut frame: serde_json::Value =
        serde_json::from_str(text).context("客户端 WebSocket 帧不是 JSON")?;
    let client_model = frame
        .get("model")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string);
    let settings = app.settings.lock().await.clone();
    if let Some(model) = settings.forced_model() {
        ws_bridge::rewrite_model_in_ws_frame(&mut frame, model);
    }
    if frame.get("type").is_none() {
        frame["type"] = serde_json::json!("response.create");
    }
    let identity = app.vm_identity.lock().await.clone();
    identity::rewrite_client_metadata_value(&mut frame, &identity);
    Ok((frame, client_model))
}

/// Who a WebSocket turn is billed to and logged as.
struct BillingAccount {
    id: String,
    email: Option<String>,
}

/// The account a WebSocket turn runs as, decided the same way as for HTTP
/// turns: the credentials Kit applies, unless the client explicitly uses
/// another account with its own credentials (then it is not billed).
async fn ws_billing_account(app: &App, client_headers: &HeaderMap) -> Option<BillingAccount> {
    let home = app.settings.lock().await.codex_home.clone();
    let (creds, override_headers) = app.sync_request_identity(Path::new(&home)).await?;
    (override_headers || !login::credentials_conflict_headers(client_headers, &creds)).then(|| {
        BillingAccount {
            id: logs::safe_text(&creds.account_id, 128),
            email: creds
                .email
                .as_deref()
                .map(|email| logs::safe_text(email, 254)),
        }
    })
}

async fn current_ws_dial(app: &App, client_headers: &HeaderMap, model: &str) -> Result<WsDial> {
    business_ws_dial(app, client_headers, model, false).await
}

/// The dial the pool keeps warm: Kit's own login, since there is no client
/// request to take headers from.
async fn warm_ws_dial(app: &App) -> Result<WsDial> {
    let settings = app.settings.lock().await.clone();
    anyhow::ensure!(
        ws_bridge::uses_websocket(&settings.upstream),
        "上游不走 WebSocket"
    );
    let model = settings
        .forced_model()
        .map(str::to_string)
        .unwrap_or_else(|| app.ws_upstream.last_model());
    business_ws_dial(app, &HeaderMap::new(), &model, true).await
}

async fn business_ws_dial(
    app: &App,
    client_headers: &HeaderMap,
    model: &str,
    require_login: bool,
) -> Result<WsDial> {
    let settings = app.settings.lock().await.clone();
    let mut headers = client_headers.clone();
    match app
        .sync_request_identity(Path::new(&settings.codex_home))
        .await
    {
        Some((creds, override_headers)) if override_headers || require_login => {
            if !login::apply_chatgpt_credentials_headers(&mut headers, &creds) {
                anyhow::bail!("无法应用 ChatGPT 鉴权请求头");
            }
        }
        None if require_login => anyhow::bail!("尚未登录 ChatGPT"),
        _ => {}
    }
    let template = resolved_proxy(&settings, &app.mihomo);
    if settings.outbound_mode == OutboundMode::Mihomo && template.trim().is_empty() {
        anyhow::bail!(
            "{}",
            app.mihomo
                .proxy_url()
                .err()
                .map(|err| err.to_string())
                .unwrap_or_else(|| "订阅节点正在连接，请稍候。".into())
        );
    }
    let proxy = app.ws_upstream.resolve_proxy(&template, None).await?;
    let identity = app.vm_identity.lock().await.clone();
    ws_dial(&settings.upstream, &proxy, &headers, &identity, model)
}

async fn finish_client_ws_turn(
    app: &App,
    account: Option<&BillingAccount>,
    model: &str,
    started: Instant,
    billing: Option<BillingRequest>,
    metrics: &mut logs::ResponseBodyMetrics,
    failed: bool,
) {
    if let Some(request) = billing {
        let usage_complete = metrics.usage_seen()
            && metrics.input_tokens().is_some()
            && metrics.output_tokens().is_some();
        request.settle(UsageOutcome {
            state: if failed {
                UsageState::Interrupted
            } else if usage_complete {
                UsageState::Measured
            } else {
                UsageState::MissingUsage
            },
            finished_at: Some(chrono::Utc::now().to_rfc3339()),
            http_status: Some(if failed { 502 } else { 200 }),
            response_model: metrics.upstream_response_model().map(str::to_owned),
            usage: metrics.token_usage(),
            usage_source: metrics.usage_seen().then(|| "provider_response".into()),
            error_kind: failed.then(|| "ws_upstream".into()),
            service_tier: metrics.service_tier().map(str::to_owned),
            first_token_ms: metrics.first_token_ms().map(|ms| ms as u64),
            transport: Some("ws_to_ws".into()),
            downgrade_signals: metrics.downgrade_signals().clone(),
        });
    }
    let settings = app.settings.lock().await.clone();
    let proxy = resolved_proxy(&settings, &app.mihomo);
    let mut details = business_network_details(
        &settings,
        &settings.upstream,
        &outbound::outbound_proxy_for_client(&proxy),
    );
    details.transport = "ws_to_ws".into();
    details.flow = "business".into();
    details.model = Some(logs::safe_text(model, 80)).filter(|value| !value.is_empty());
    details.http_version = Some("websocket".into());
    details.response_header_ms = Some(started.elapsed().as_millis());
    details.output_tokens = metrics.output_tokens();
    details.first_token_ms = metrics.first_token_ms();
    details.account_id = account.map(|account| account.id.clone());
    details.account_email = account.and_then(|account| account.email.clone());
    details.in_progress = false;
    if failed {
        details.error_kind = Some("ws_upstream".into());
    }
    app.record(
        "WS",
        "/responses",
        if failed { 502 } else { 200 },
        started,
        details,
    )
    .await;
}

fn is_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP
        .iter()
        .any(|h| name.as_str().eq_ignore_ascii_case(h))
}

fn request_account(headers: &HeaderMap) -> Option<String> {
    headers
        .get("chatgpt-account-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
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
mod tests {
    use super::*;

    #[test]
    fn mihomo_without_sidecar_does_not_use_saved_manual_proxy() {
        let settings = Settings {
            outbound_mode: OutboundMode::Mihomo,
            outbound_proxy: "http://127.0.0.1:7890".into(),
            ..Settings::default()
        };
        let mihomo = MihomoRuntime::default();
        assert_eq!(resolved_proxy(&settings, &mihomo), "");
        let mut manual = settings;
        manual.outbound_mode = OutboundMode::Manual;
        assert_eq!(resolved_proxy(&manual, &mihomo), "http://127.0.0.1:7890");
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
    fn dropped_billing_request_is_recovered_as_interrupted() {
        let store = BillingStore::open_in_memory().unwrap();
        let request_id = "dropped-billing-request".to_string();
        store
            .begin_request(RequestStart {
                request_id: request_id.clone(),
                provider: "chatgpt".into(),
                account_id: "account-a".into(),
                email: None,
                source: "business".into(),
                started_at: chrono::Utc::now().to_rfc3339(),
                requested_model: Some("gpt-test".into()),
                sent_model: Some("gpt-test".into()),
                service_tier: None,
            })
            .unwrap();
        {
            let _request = BillingRequest::new(Arc::new(store.clone()), request_id.clone());
        }
        assert_eq!(
            store.get_by_id(&request_id).unwrap().unwrap().state,
            UsageState::Interrupted
        );
    }

    #[tokio::test]
    async fn request_identity_overrides_the_client_headers() {
        if std::env::var_os("CSK_IDENTITY_TEST_CHILD").is_none() {
            let dir = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "proxy::tests::request_identity_overrides_the_client_headers",
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
        let (creds, override_headers) = app.sync_request_identity(&home).await.unwrap();
        assert!(override_headers);
        assert_eq!(creds.account_id, "new-account");

        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer old-access"),
        );
        assert!(!login::credentials_match_headers(&headers, &creds));
        login::apply_chatgpt_credentials_headers(&mut headers, &creds);
        assert!(login::credentials_match_headers(&headers, &creds));
    }

    #[test]
    fn same_network_keeps_session_placeholder_until_bound() {
        let settings = Settings {
            outbound_proxy:
                "socks5://xmtt1126849-region-DE-sid-{session}-t-120:pass@us.arxlabs.io:3010".into(),
            outbound_mode: OutboundMode::Manual,
            ..Settings::default()
        };
        let template = resolved_proxy(&settings, &MihomoRuntime::default());
        assert!(outbound::has_session_placeholder(&template));
        assert!(outbound::apply_bound_session(&template, None).is_err());
        assert!(outbound::apply_bound_session(&template, Some("1Z5jzVPs"))
            .unwrap()
            .contains("-sid-1Z5jzVPs-t-120"));
        assert!(business_http_client(&template, None).is_ok());
        assert!(business_http_client(&template, Some("1Z5jzVPs")).is_ok());
    }

    #[tokio::test]
    async fn business_forward_replaces_client_identity() {
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel::<(HeaderMap, Vec<u8>)>();
        let upstream = axum::Router::new().fallback(move |req: Request<Body>| {
            let sent = sent.clone();
            async move {
                let (parts, body) = req.into_parts();
                let bytes = axum::body::to_bytes(body, 64 * 1024).await.unwrap();
                sent.send((parts.headers, bytes.to_vec())).unwrap();
                "ok"
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Arc::new(
            App::new(Settings {
                upstream: format!("http://{}", listener.local_addr().unwrap()),
                ..Settings::default()
            })
            .unwrap(),
        );
        let identity = app.vm_identity.lock().await.clone();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let response = proxy_http(
            app.clone(),
            Request::builder()
                .method("POST")
                .uri("/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header("user-agent", "client-ua")
                .header("originator", "client-origin")
                .header("version", "9.9.9")
                .header("session_id", "client-session")
                .header("x-codex-installation-id", "client-install")
                .header("x-codex-window-id", "client-window")
                .header("x-codex-routing-hint", "model=client")
                .header("x-codex-turn-metadata", "client-turn")
                .header(header::COOKIE, "__cf_bm=client")
                .body(Body::from(
                    r#"{"model":"gpt-test","previous_response_id":"resp_1","client_metadata":{"x-codex-installation-id":"client-install","session_id":"client-session","x-codex-window-id":"client-window","thread_id":"thread-keep","turn_id":"turn-keep"}}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let (headers, body) = received.recv().await.unwrap();
        assert_eq!(headers["user-agent"], identity.user_agent());
        assert_eq!(headers["originator"], identity.originator);
        assert_eq!(headers["version"], identity.cli_version);
        assert_eq!(headers["session_id"], identity.session_id);
        assert_eq!(headers["x-codex-installation-id"], identity.installation_id);
        assert_eq!(headers["x-codex-window-id"], identity.window_id);
        assert_eq!(headers["x-codex-routing-hint"], "model=gpt-test");
        assert!(headers.get("x-codex-turn-metadata").is_none());
        assert!(headers.get(header::COOKIE).is_none());
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value.get("previous_response_id").is_none());
        let metadata = value["client_metadata"].as_object().unwrap();
        assert_eq!(
            metadata["x-codex-installation-id"],
            identity.installation_id
        );
        assert_eq!(metadata["session_id"], identity.session_id);
        assert_eq!(metadata["x-codex-window-id"], identity.window_id);
        assert_eq!(metadata["thread_id"], "thread-keep");
        assert_eq!(metadata["turn_id"], "turn-keep");
        server.abort();
    }

    #[test]
    fn ws_dial_uses_the_vm_identity() {
        let identity = VmIdentity::ephemeral();
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("chatgpt-account-id", "acct".parse().unwrap());
        headers.insert("user-agent", "client-ua".parse().unwrap());
        headers.insert("x-codex-installation-id", "client-install".parse().unwrap());
        let dial = ws_dial(
            "https://chatgpt.com/backend-api/codex/responses",
            "",
            &headers,
            &identity,
            "gpt-test",
        )
        .unwrap();
        let extra = |name: &str| {
            dial.extra_headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .unwrap()
        };
        assert_eq!(dial.authorization, "Bearer secret");
        assert_eq!(dial.account_id, "acct");
        assert_eq!(extra("user-agent"), identity.user_agent());
        assert_eq!(extra("x-codex-installation-id"), identity.installation_id);
        assert_eq!(extra("session_id"), identity.session_id);
        assert_eq!(extra("x-codex-window-id"), identity.window_id);
        assert_eq!(extra("thread-id"), identity.thread_id);
        assert_eq!(extra("x-client-request-id"), identity.thread_id);
        assert_eq!(extra("x-codex-routing-hint"), "model=gpt-test");
        assert!(!extra("user-agent").contains("client-ua"));
    }

    #[tokio::test]
    async fn client_ws_frame_replaces_device_metadata() {
        let app = App::new(Settings {
            forced_model: "gpt-forced".into(),
            ..Settings::default()
        })
        .unwrap();
        let identity = app.vm_identity.lock().await.clone();
        let (frame, client_model) = prepare_client_ws_frame(
            &app,
            r#"{"type":"response.create","model":"gpt-test","client_metadata":{"x-codex-installation-id":"client","session_id":"client","x-codex-window-id":"client","thread_id":"keep","turn_id":"turn"}}"#,
        )
        .await
        .unwrap();
        assert_eq!(
            frame["client_metadata"]["x-codex-installation-id"],
            identity.installation_id
        );
        assert_eq!(frame["client_metadata"]["session_id"], identity.session_id);
        assert_eq!(
            frame["client_metadata"]["x-codex-window-id"],
            identity.window_id
        );
        assert_eq!(frame["client_metadata"]["thread_id"], "keep");
        assert_eq!(frame["client_metadata"]["turn_id"], "turn");
        // The forced model is sent; the client's own model is kept for records.
        assert_eq!(frame["model"], "gpt-forced");
        assert_eq!(client_model.as_deref(), Some("gpt-test"));
    }

    #[tokio::test]
    async fn websocket_turns_are_billed_to_kits_credentials() {
        const TEST: &str = "proxy::tests::websocket_turns_are_billed_to_kits_credentials";
        if std::env::var_os("CSK_WS_BILLING_TEST_CHILD").is_none() {
            let dir = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env("CSK_WS_BILLING_TEST_CHILD", "1")
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
        use base64::Engine as _;
        let home = tempfile::tempdir().unwrap();
        let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({"chatgpt_account_id":"account-a","email":"a@example.com"})
                .to_string(),
        );
        std::fs::write(
            login::kit_auth_path(home.path()),
            serde_json::json!({
                "auth_mode": "chatgpt",
                "tokens": {
                    "id_token": format!("e30.{claims}.signature"),
                    "access_token": "access-a",
                    "refresh_token": "refresh-a",
                    "account_id": "account-a"
                }
            })
            .to_string(),
        )
        .unwrap();
        let app = App::new(Settings {
            codex_home: home.path().display().to_string(),
            ..Settings::default()
        })
        .unwrap();
        // Codex still sends the account it had before a switch in Kit: the
        // turn runs as Kit's account, so it is billed there, with the email.
        let mut headers = HeaderMap::new();
        headers.insert(
            "chatgpt-account-id",
            HeaderValue::from_static("stale-account"),
        );
        let account = ws_billing_account(&app, &headers).await.unwrap();
        assert_eq!(account.id, "account-a");
        assert_eq!(account.email.as_deref(), Some("a@example.com"));
        let billing = app
            .begin_ws_billing(
                Instant::now(),
                &account,
                Some("gpt-client"),
                Some("gpt-forced"),
                Some("priority".into()),
            )
            .unwrap();
        let record = app.billing.get_by_id(&billing.request_id).unwrap().unwrap();
        assert_eq!(record.account_id, "account-a");
        assert_eq!(record.email.as_deref(), Some("a@example.com"));
        assert_eq!(record.requested_model.as_deref(), Some("gpt-client"));
        assert_eq!(record.sent_model.as_deref(), Some("gpt-forced"));
        drop(billing);
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
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn rerouted_sse_response_is_recorded_as_downgraded() {
        const TEST: &str = "proxy::tests::rerouted_sse_response_is_recorded_as_downgraded";
        if std::env::var_os("CSK_DOWNGRADE_TEST_CHILD").is_none() {
            let dir = tempfile::tempdir().unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", TEST, "--nocapture"])
                .env("CSK_DOWNGRADE_TEST_CHILD", "1")
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
                    "access_token":"dg-access",
                    "refresh_token":"dg-refresh",
                    "account_id":"account-a"
                }
            })
            .to_string(),
        )
        .unwrap();
        let upstream = axum::Router::new().fallback(|| async {
            (
                [
                    (header::CONTENT_TYPE, "text/event-stream"),
                    (HeaderName::from_static("openai-model"), "gpt-5.6-luna"),
                    (
                        HeaderName::from_static("x-codex-safety-buffering-enabled"),
                        "true",
                    ),
                    (
                        HeaderName::from_static("x-codex-safety-buffering-faster-model"),
                        "gpt-5.6-luna",
                    ),
                ],
                concat!(
                    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\"}}\n\n",
                    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\",\"safety_buffering\":{\"use_cases\":[\"cyber\"],\"reasons\":[\"user_risk\"]}}\n\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-6-astra\",\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
                ),
            )
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let app = Arc::new(
            App::new(Settings {
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
                .body(Body::from(r#"{"model":"gpt-6-astra","stream":true}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(event) = app.billing.last_downgrade() {
                    break event;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let report = event.report;
        assert_eq!(report.verdict, crate::downgrade::Verdict::Confirmed);
        assert_eq!(report.requested_model.as_deref(), Some("gpt-6-astra"));
        assert_eq!(report.effective_model.as_deref(), Some("gpt-5.6-luna"));
        assert!(report.safety_buffering);
        assert_eq!(report.use_cases, ["cyber"]);
        let record = app.billing.get_by_id(&event.request_id).unwrap().unwrap();
        assert!(record.downgrade.is_some());
        server.abort();
    }
}
