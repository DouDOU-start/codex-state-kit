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
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex, Notify, RwLock};
use tokio::task::JoinHandle;
use url::Url;

use crate::accounts::{self, AccountEnvironment, NetworkProfile};
use crate::attach::{self, is_attached};
use crate::basispoints;
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
use crate::settings::{save_settings, OutboundMode, Settings, SettingsPatch, UpstreamMode};
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

async fn debug_log(msg: String) {
    eprintln!("{}", msg);
    let path = crate::settings::home_dir().join(if cfg!(debug_assertions) {
        ".codex-state-kit-dev-debug.log"
    } else {
        ".codex-state-kit-debug.log"
    });
    // Keep request forwarding off the synchronous filesystem path. The debug
    // line is best-effort and can be dropped if the runtime is shutting down.
    tokio::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                let ts = chrono::Local::now().format("%H:%M:%S%.3f");
                let _ = writeln!(f, "[{}] {}", ts, msg);
            }
        })
        .await;
    });
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
    pub upstream_mode: UpstreamMode,
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
    /// Coordinates identity reads with exclusive settings/route transitions.
    transition: RwLock<()>,
    /// Cached auth snapshot for concurrent forwarding. The transition lock
    /// still makes account/settings changes linearizable; file stamps avoid
    /// reparsing both auth JSON files for every request.
    request_identity_cache: Mutex<Option<RequestIdentityCache>>,
    http: Mutex<PooledUpstream>,
    pub sidecar_wake: Notify,
    vm_identity: Mutex<VmIdentity>,
    ws_upstream: WsUpstreamPool,
    basispoints: Arc<Mutex<basispoints::BpsState>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AuthFileStamp {
    modified: Option<SystemTime>,
    len: Option<u64>,
}

#[derive(Clone, Debug)]
struct RequestIdentityCache {
    home: std::path::PathBuf,
    kit: AuthFileStamp,
    official: AuthFileStamp,
    value: Option<(login::ChatGptCredentials, bool)>,
}

fn auth_file_stamp(path: &Path) -> AuthFileStamp {
    match std::fs::metadata(path) {
        Ok(metadata) => AuthFileStamp {
            modified: metadata.modified().ok(),
            len: Some(metadata.len()),
        },
        Err(_) => AuthFileStamp {
            modified: None,
            len: None,
        },
    }
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
            transition: RwLock::new(()),
            request_identity_cache: Mutex::new(None),
            http: Mutex::new(http),
            sidecar_wake: Notify::new(),
            vm_identity: Mutex::new(if cfg!(test) {
                VmIdentity::ephemeral()
            } else {
                VmIdentity::load_or_create()
            }),
            ws_upstream: WsUpstreamPool::new(),
            basispoints: Arc::new(Mutex::new(basispoints::BpsState::default())),
        })
    }

    /// The credentials for the next upstream request, and whether Kit must
    /// override the client's own auth headers with them.
    async fn sync_request_identity(
        &self,
        home: &Path,
    ) -> Option<(login::ChatGptCredentials, bool)> {
        let _transition = self.transition.read().await;
        let kit = auth_file_stamp(&login::kit_auth_path(home));
        let official = auth_file_stamp(&home.join("auth.json"));
        {
            let cache = self.request_identity_cache.lock().await;
            if let Some(cached) = cache.as_ref().filter(|cached| {
                cached.home == home && cached.kit == kit && cached.official == official
            }) {
                return cached.value.clone();
            }
        }
        let value = login::request_credentials(home).ok();
        *self.request_identity_cache.lock().await = Some(RequestIdentityCache {
            home: home.to_path_buf(),
            kit,
            official,
            value: value.clone(),
        });
        value
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
            upstream_mode: settings.upstream_mode,
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
    #[allow(dead_code)]
    geo_cache: Arc<Mutex<Option<(String, Instant, Option<outbound::ProxyGeo>)>>>,
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
            geo_cache: Arc::new(Mutex::new(None)),
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
        if let Some(environment) = &profile.environment {
            environment.validate()?;
        }
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
        self.apply_settings_to(patch.into_settings()?).await?;
        // Outbound edits belong to the account that is currently live.
        self.remember_account_environment().await;
        Ok(self.managed_status().await)
    }

    /// The environment Kit is using right now: virtual device and outbound line.
    async fn current_environment(&self) -> AccountEnvironment {
        let _settings_guard = self.settings_change.lock().await;
        let settings = self.app.settings.lock().await.clone();
        let mihomo = self.app.mihomo.status();
        let detected_geo: Option<outbound::ProxyGeo> = {
            #[cfg(test)]
            {
                None
            }
            #[cfg(not(test))]
            {
                let auto_region = {
                    let vm = self.app.vm_identity.lock().await;
                    vm.enabled && vm.environment.auto_region
                };
                if auto_region {
                    let template = resolved_proxy(&settings, &self.app.mihomo);
                    if template.trim().is_empty() {
                        None
                    } else {
                        let proxy = self
                            .app
                            .ws_upstream
                            .resolve_proxy(&template, None)
                            .await
                            .unwrap_or_default();
                        let key = format!(
                            "{}|{:?}|{:?}",
                            proxy,
                            mihomo.selected,
                            mihomo
                                .groups
                                .iter()
                                .map(|g| (&g.name, &g.now))
                                .collect::<Vec<_>>()
                        );
                        let mut cache = self.geo_cache.lock().await;
                        if let Some((_, _, geo)) = cache.as_ref().filter(|(k, at, geo)| {
                            *k == key
                                && at.elapsed()
                                    < Duration::from_secs(if geo.is_some() { 900 } else { 60 })
                        }) {
                            geo.clone()
                        } else {
                            let geo = outbound::detect_proxy_geo(&proxy).await.ok();
                            if geo.is_none() {
                                eprintln!("[identity] 代理出口地区探测失败，保留现有环境");
                            }
                            *cache = Some((key, Instant::now(), geo.clone()));
                            geo
                        }
                    }
                } else {
                    None
                }
            }
        };
        let vm = {
            let mut vm = self.app.vm_identity.lock().await;
            let current_mihomo = self.app.mihomo.status();
            let same_node = current_mihomo.selected == mihomo.selected
                && current_mihomo
                    .groups
                    .iter()
                    .map(|g| (&g.name, &g.now))
                    .eq(mihomo.groups.iter().map(|g| (&g.name, &g.now)));
            if let Some(geo) =
                detected_geo.filter(|_| vm.enabled && vm.environment.auto_region && same_node)
            {
                let mut environment = vm.environment.clone();
                environment.region = geo.country_code.unwrap_or_default().to_ascii_uppercase();
                environment.timezone = geo.timezone.unwrap_or_default();
                environment.locale = geo
                    .languages
                    .as_deref()
                    .and_then(|value| value.split(',').next())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| value.replace('_', "-"))
                    .unwrap_or_default();
                if environment.validate().is_ok() {
                    vm.environment = environment;
                }
            }
            #[cfg(not(test))]
            if let Err(err) = vm.save() {
                eprintln!("[identity] 保存环境失败: {err:#}");
            }
            vm.clone()
        };
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
        // The Kit group's saved choice is the node to start on; older saved
        // lines may still carry a stale `mihomo_node`.
        let mihomo_node = network
            .mihomo_selections
            .get(crate::mihomo::GROUP)
            .cloned()
            .unwrap_or_else(|| network.mihomo_node.clone());
        let network_changed = next.outbound_mode != network.outbound_mode
            || next.outbound_proxy != network.outbound_proxy
            || next.mihomo_subscription != network.mihomo_subscription
            || next.mihomo_node != mihomo_node;
        if network_changed {
            next.outbound_mode = network.outbound_mode;
            next.outbound_proxy = network.outbound_proxy.clone();
            next.mihomo_subscription = network.mihomo_subscription.clone();
            next.mihomo_node = mihomo_node;
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
                self.current_environment().await
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

    /// Selects a subscription node and remembers it. The Kit group's choice
    /// is saved as `mihomo_node`, which the core starts on after a restart;
    /// before, only the running core changed and a restart fell back to the
    /// first node.
    pub async fn select_mihomo_node(&self, group: &str, node: &str) -> Result<()> {
        self.app.mihomo.select_in_group(group, node).await?;
        if group == crate::mihomo::GROUP {
            let _change = self.settings_change.lock().await;
            let mut settings = self.app.settings.lock().await;
            if settings.mihomo_node != node {
                let mut next = settings.clone();
                next.mihomo_node = node.to_string();
                save_settings(&next)?;
                *settings = next;
            }
        }
        // Connections opened through the previous node use the old exit.
        self.app.ws_upstream.invalidate().await;
        // The selected node is part of the live account's outbound line.
        self.remember_account_environment().await;
        Ok(())
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
            Some(self.app.transition.write().await)
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
        let mut last_environment_refresh = Instant::now();
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
            if last_environment_refresh.elapsed() >= Duration::from_secs(60) {
                self.remember_account_environment().await;
                last_environment_refresh = Instant::now();
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
        let target = if kind == "mihomo" {
            crate::latency::NODE_PROBE_TARGET.to_string()
        } else {
            crate::latency::probe_target(&settings.upstream)?
        };
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
            "mihomo_codex" => {
                let proxy = resolved_proxy(&settings, &self.app.mihomo);
                vec![crate::latency::sample_from_result(
                    "Codex 链路",
                    crate::latency::probe_through_proxy(&proxy, &target).await,
                )]
            }
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
        if app.settings.lock().await.upstream_mode == UpstreamMode::Basispoints {
            return proxy_bps_ws(app, req).await;
        }
        return proxy_ws(app, req).await;
    }
    proxy_http(app, req).await
}

/// Accept a Codex WebSocket client while keeping the BPS leg on HTTP SSE.
/// This lets the existing Codex attachment continue to work when the client
/// elects its native WebSocket transport.
async fn proxy_bps_ws(app: Arc<App>, req: Request<Body>) -> Response {
    let headers = req.headers().clone();
    let (mut parts, _body) = req.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade.on_upgrade(move |socket| async move {
            if let Err(error) = client_bps_ws_session(app, headers, socket).await {
                eprintln!("[bps-ws] 客户端会话结束: {error:#}");
            }
        }),
        Err(rejection) => rejection.into_response(),
    }
}

async fn client_bps_ws_session(
    app: Arc<App>,
    client_headers: HeaderMap,
    mut socket: WebSocket,
) -> Result<()> {
    let mut history = basispoints::WsHistory::default();
    while let Some(message) = socket.recv().await {
        let message = message.context("读取 BPS WebSocket 客户端消息")?;
        let text = match message {
            WsMessage::Text(text) => text.to_string(),
            WsMessage::Close(_) => return Ok(()),
            WsMessage::Ping(payload) => {
                socket.send(WsMessage::Pong(payload)).await.ok();
                continue;
            }
            WsMessage::Pong(_) | WsMessage::Binary(_) => continue,
        };
        let request_body = match history.expand(&text) {
            Ok(body) => body,
            Err(_) => {
                socket.send(WsMessage::text(json!({"type":"error", "status":400, "error":{"type":"invalid_request_error","code":"previous_response_not_found","message":"Send a full response.create request without previous_response_id"}}).to_string())).await?;
                continue;
            }
        };
        if request_body
            .get("generate")
            .and_then(serde_json::Value::as_bool)
            == Some(false)
        {
            let response = json!({"id":format!("resp_{}",uuid::Uuid::new_v4().simple()),"object":"response","status":"completed","output":[],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}});
            history.remember(&request_body, &response);
            socket
                .send(WsMessage::text(
                    json!({"type":"response.completed","response":response}).to_string(),
                ))
                .await?;
            continue;
        }
        let mut builder = Request::builder()
            .method(http::Method::POST)
            .uri("/responses");
        for (name, value) in &client_headers {
            if is_hop(name)
                || name.as_str().eq_ignore_ascii_case("upgrade")
                || name.as_str().starts_with("sec-websocket-")
            {
                continue;
            }
            builder = builder.header(name, value);
        }
        builder = builder.header(header::ACCEPT, "text/event-stream");
        let request = builder
            .body(Body::from(serde_json::to_vec(&request_body)?))
            .context("构造 BPS HTTP 请求")?;
        let response = proxy_http(app.clone(), request).await;
        let status = response.status();
        let mut body = response.into_body().into_data_stream();
        if !status.is_success() {
            let mut error_body = Vec::new();
            while let Some(chunk) = body.next().await {
                error_body.extend_from_slice(&chunk.context("读取 BPS 错误响应")?);
            }
            let payload = String::from_utf8_lossy(&error_body).to_string();
            socket.send(WsMessage::text(payload)).await.ok();
            continue;
        }
        let mut pending = Vec::new();
        while let Some(chunk) = body.next().await {
            pending.extend_from_slice(&chunk.context("读取 BPS SSE")?);
            while let Some((pos, separator_len)) = pending
                .windows(4)
                .position(|pair| pair == b"\r\n\r\n")
                .map(|pos| (pos, 4))
                .or_else(|| {
                    pending
                        .windows(2)
                        .position(|pair| pair == b"\n\n")
                        .map(|pos| (pos, 2))
                })
            {
                let block: Vec<u8> = pending.drain(..pos + separator_len).collect();
                if let Some(data) = String::from_utf8_lossy(&block)
                    .lines()
                    .find_map(|line| line.strip_prefix("data:").map(str::trim))
                {
                    if let Ok(event) = serde_json::from_str::<serde_json::Value>(data) {
                        if event.get("type").and_then(serde_json::Value::as_str)
                            == Some("response.completed")
                        {
                            if let Some(response) = event.get("response") {
                                history.remember(&request_body, response);
                            }
                        }
                    }
                    socket.send(WsMessage::text(data.to_string())).await?;
                }
            }
        }
    }
    Ok(())
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
            let stream: Pin<
                Box<dyn futures_util::Stream<Item = Result<Bytes, axum::Error>> + Send>,
            > = Box::pin(body.into_data_stream().map(|result| {
                result.map_err(|error| axum::Error::new(std::io::Error::other(error.to_string())))
            }));
            let stream = futures_util::stream::unfold(
                (stream, tracker),
                move |(mut stream, mut tracker)| async move {
                    if tracker.finished {
                        return None;
                    }
                    let chunks = tracker
                        .lifecycle
                        .as_ref()
                        .map(|lifecycle| lifecycle.snapshot().stream_chunks)
                        .unwrap_or(0);
                    let mut upstream_eof_incomplete = false;
                    let mut next = match tokio::time::timeout(
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
                        Some(Err(error)) => {
                            if let Some(lifecycle) = &tracker.lifecycle {
                                lifecycle.error();
                            }
                            tracker.metrics.set_error_message(&error.to_string());
                            tracker.entry.error_kind = Some("response_body".into());
                            tracker.finished = true;
                        }
                        None => {
                            tracker
                                .metrics
                                .finish(tracker.started.elapsed().as_millis());
                            // A successful SSE response must carry a protocol
                            // terminal event. An HTTP EOF alone is ambiguous:
                            // it can otherwise be recorded as a successful
                            // 200 response even though the provider truncated
                            // the stream before `response.completed` (or an
                            // explicit failure/incomplete event).
                            let truncated_sse = is_sse
                                && !tracker.metrics.terminal_event_seen()
                                && tracker.metrics.error_kind.is_none();
                            if truncated_sse {
                                if let Some(lifecycle) = &tracker.lifecycle {
                                    lifecycle.error();
                                }
                                tracker.entry.error_kind = Some("upstream_eof".into());
                                tracker.finished = true;
                                upstream_eof_incomplete = true;
                            } else {
                                if let Some(lifecycle) = &tracker.lifecycle {
                                    lifecycle.complete();
                                }
                                tracker.finished = true;
                            }
                        }
                    }
                    if upstream_eof_incomplete {
                        let incomplete = Bytes::from_static(SSE_IDLE_TIMEOUT_EVENT);
                        tracker.metrics.observe(
                            incomplete.as_ref(),
                            tracker.started.elapsed().as_millis(),
                            true,
                        );
                        next = Some(Ok(incomplete));
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
                    error_message: Some(logs::safe_text(&err.to_string(), 512)),
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
            error_message: self.metrics.error_message().map(str::to_owned),
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
                    error_message: self.metrics.error_message().map(str::to_owned),
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
    let target = if request_settings.upstream_mode == UpstreamMode::Basispoints
        && parts.method == http::Method::POST
        && parts
            .uri
            .path()
            .trim_end_matches('/')
            .ends_with("/responses")
    {
        basispoints::DEFAULT_ENDPOINT.to_string()
    } else {
        join_upstream(&upstream, &parts.uri)?
    };
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
    debug_log(format!(
        "[proxy] {} {} body={}bytes encoding={}",
        parts.method,
        path,
        bytes.len(),
        details.content_encoding,
    ))
    .await;

    let request_identity = app.sync_request_identity(Path::new(&home)).await;
    if request_settings.upstream_mode == UpstreamMode::Basispoints && request_identity.is_none() {
        anyhow::bail!("Basispoints 模式需要先登录 ChatGPT");
    }
    let billing_identity_matches =
        request_identity
            .as_ref()
            .is_some_and(|(creds, override_headers)| {
                *override_headers || !login::credentials_conflict_headers(&parts.headers, creds)
            });
    if request_settings.upstream_mode == UpstreamMode::Basispoints {
        if let Some((creds, _)) = &request_identity {
            if !login::apply_chatgpt_credentials_headers(&mut parts.headers, creds) {
                anyhow::bail!("无法应用 ChatGPT 鉴权请求头");
            }
        }
    } else if let Some((creds, true)) = &request_identity {
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
    if request_settings.upstream_mode == UpstreamMode::Basispoints {
        if let Some((creds, _)) = &request_identity {
            details.account_id = Some(logs::safe_text(&creds.account_id, 128));
            details.account_email = creds
                .email
                .as_deref()
                .map(|email| logs::safe_text(email, 254));
        }
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
    let root_vm = app.vm_identity.lock().await.clone();
    let mut request_context =
        identity::request_context_from_body(&bytes, content_encoding.as_deref())
            .unwrap_or_default();
    merge_request_context_from_headers(&mut request_context, &parts.headers);
    // Scope the generated session/window IDs to this client task. A request
    // without source metadata gets a unique HTTP scope; a request carrying a
    // Codex session/window deterministically reuses that virtual scope.
    let request_scope = format!("http:{}", uuid::Uuid::new_v4());
    let vm = root_vm.scoped_for_request(&request_context, &request_scope);
    if vm.enabled {
        match identity::rewrite_client_metadata_in_body_with_context(
            &bytes,
            content_encoding.as_deref(),
            &vm,
            Some(&request_context),
        ) {
            Ok(rewritten) => {
                bytes = rewritten.into();
                request_context =
                    identity::request_context_from_body(&bytes, content_encoding.as_deref())
                        .unwrap_or(request_context);
                merge_request_context_from_headers(&mut request_context, &parts.headers);
                details.body_bytes = bytes.len();
            }
            Err(err) => {
                eprintln!("[identity] 请求体身份改写失败，保留原正文: {err}");
            }
        }
    }
    // HTTP clients stay on the native HTTP SSE path.  The HTTP→WebSocket
    // bridge can receive the generated content but lose the upstream
    // `response.completed` usage event when the WS closes, which leaves the
    // durable billing row interrupted and impossible to price.  Native SSE
    // carries the complete usage record reliably.  Real WebSocket clients
    // still use the upstream WebSocket path in `proxy_ws` below.
    // WebSocket 链式续跑才认 previous_response_id；走 HTTP 时必须去掉，
    // 否则上游返回 "Invalid previous_response_id"。
    let mut bps_lineage = None;
    if parts.method == http::Method::POST && path.contains("/responses") {
        let stripped =
            crate::body_model::strip_previous_response_id(&bytes, content_encoding.as_deref());
        if stripped.as_slice() != bytes.as_ref() {
            bytes = stripped.into();
            details.body_bytes = bytes.len();
        }
        if request_settings.upstream_mode == UpstreamMode::Basispoints {
            let account_id = request_identity
                .as_ref()
                .map(|(credentials, _)| credentials.account_id.as_str());
            let prepared =
                basispoints::prepare_request(&app.basispoints, &bytes, account_id).await?;
            bytes = prepared.body.into();
            bps_lineage = Some(prepared.lineage);
            details.body_bytes = bytes.len();
            parts.headers.remove(header::CONTENT_ENCODING);
            parts.headers.remove(header::CONTENT_LENGTH);
            parts.headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
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
            || name.as_str().starts_with("sec-websocket-")
            || (vm.enabled && identity::is_vm_identity_header(name.as_str()))
            || name.as_str().eq_ignore_ascii_case("x-codex-turn-state")
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    if vm.enabled {
        builder = builder
            .header("user-agent", vm.user_agent())
            .header("originator", &vm.originator)
            .header("version", &vm.cli_version)
            .header("x-codex-installation-id", &vm.installation_id)
            .header("x-codex-window-id", &vm.window_id)
            // `session-id`/`thread-id` are the official Codex headers.
            .header("session-id", &vm.session_id);
        let thread_id = request_context
            .thread_id
            .as_deref()
            .unwrap_or(vm.thread_id.as_str());
        builder = builder
            .header("thread-id", thread_id)
            .header("x-client-request-id", thread_id);
        if let Some(parent_thread_id) = request_context.parent_thread_id.as_deref() {
            builder = builder.header("x-codex-parent-thread-id", parent_thread_id);
        }
        if let Some(subagent) = request_context.subagent.as_deref() {
            builder = builder.header("x-openai-subagent", subagent);
        }
        if let Some(turn_metadata) = request_context.turn_metadata.as_deref() {
            if turn_metadata.len() <= 8192
                && turn_metadata.is_ascii()
                && !turn_metadata.chars().any(char::is_control)
            {
                builder = builder.header("x-codex-turn-metadata", turn_metadata);
            }
        }
        if let Some(model) = request_model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty() && !model.chars().any(char::is_control))
        {
            builder = builder.header("x-codex-routing-hint", vm.routing_hint(model));
        }
    }
    if request_settings.upstream_mode == UpstreamMode::Basispoints {
        let account_id = parts
            .headers
            .get("chatgpt-account-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        builder = builder
            .header("x-openai-account-id", account_id)
            .header("x-basispoints-auth-mode", "chatgpt")
            // Basispoints selects its Excel tool surface from these client
            // identity headers. Without them it may expose unrelated native
            // tools such as web_search instead of run_officejs.
            .header(
                "x-openai-internal-basispoints-client-agent-profile",
                "excel",
            )
            .header("x-openai-internal-basispoints-client-editor", "excel")
            .header("x-openai-internal-basispoints-client-host", "office")
            .header("x-openai-internal-basispoints-client-platform", "excel")
            .header("x-openai-internal-basispoints-client-platform-class", "PC")
            .header(
                "x-openai-internal-basispoints-client-product",
                "basispoints-excel-plugin",
            )
            .header("x-openai-internal-basispoints-client-runtime", "desktop")
            .header("x-openai-internal-basispoints-office-host", "Excel")
            .header("x-openai-internal-basispoints-office-platform", "PC");
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

    debug_log(format!(
        "[resp] {} {} → {}",
        parts.method, path, resp_status_u16
    ))
    .await;

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
    // Preserve the upstream error body and log a bounded, redacted summary.
    // Without this, a BPS 422 only appeared as a status code in dev logs,
    // making malformed input and authentication failures indistinguishable.
    if request_settings.upstream_mode == UpstreamMode::Basispoints && !status.is_success() {
        let error_body = upstream_resp.bytes().await.unwrap_or_default();
        let summary = String::from_utf8_lossy(&error_body);
        let summary = logs::safe_text(summary.trim(), 2048);
        debug_log(format!(
            "[bps] upstream error status={} body={}",
            status, summary
        ))
        .await;
        let mut response = Response::new(Body::from(error_body));
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        return Ok(response);
    }
    let body_stream = upstream_resp.bytes_stream();
    let body = if let Some(lineage) =
        bps_lineage.filter(|_| status.is_success() && details.transport == "http_sse")
    {
        Body::from_stream(basispoints::sse_stream(
            body_stream,
            app.basispoints.clone(),
            lineage,
        ))
    } else {
        Body::from_stream(body_stream)
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

#[allow(dead_code)]
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
    let mut request_context = identity::request_context_from_value(&frame);
    merge_request_context_from_headers(&mut request_context, headers);
    let root_identity = app.vm_identity.lock().await.clone();
    let request_scope = format!("http-ws:{}", uuid::Uuid::new_v4());
    let identity = root_identity.scoped_for_request(&request_context, &request_scope);
    identity::rewrite_client_metadata_value_with_context(
        &mut frame,
        &identity,
        Some(&request_context),
    );
    let mut request_context = identity::request_context_from_value(&frame);
    merge_request_context_from_headers(&mut request_context, headers);
    let model = frame
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    // The pool's sticky `{session}`, so turns reuse the same connections.
    let proxy = app.ws_upstream.resolve_proxy(proxy, None).await?;
    let dial = ws_dial(target, &proxy, headers, &identity, model, &request_context)?;
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
    request_context: &identity::RequestContext,
) -> Result<WsDial> {
    let url = ws_bridge::upstream_to_ws_url(target).map_err(|err| anyhow::anyhow!(err))?;
    let mut extra_headers = if identity.enabled {
        let thread_id = request_context
            .thread_id
            .as_deref()
            .unwrap_or(identity.thread_id.as_str());
        vec![
            ("user-agent".into(), identity.user_agent()),
            ("originator".into(), identity.originator.clone()),
            ("version".into(), identity.cli_version.clone()),
            (
                "x-codex-installation-id".into(),
                identity.installation_id.clone(),
            ),
            ("x-codex-window-id".into(), identity.window_id.clone()),
            ("session-id".into(), identity.session_id.clone()),
            ("thread-id".into(), thread_id.to_string()),
            ("x-client-request-id".into(), thread_id.to_string()),
        ]
    } else {
        headers
            .iter()
            .filter(|(name, _)| !is_ws_passthrough_excluded(name))
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .filter(|value| !value.chars().any(char::is_control))
                    .map(|value| (name.as_str().to_string(), value.to_string()))
            })
            .collect()
    };
    if identity.enabled {
        if let Some(parent_thread_id) = request_context.parent_thread_id.as_deref() {
            extra_headers.push((
                "x-codex-parent-thread-id".into(),
                parent_thread_id.to_string(),
            ));
        }
        if let Some(subagent) = request_context.subagent.as_deref() {
            extra_headers.push(("x-openai-subagent".into(), subagent.to_string()));
        }
        if let Some(turn_metadata) = request_context.turn_metadata.as_deref() {
            if turn_metadata.len() <= 8192
                && turn_metadata.is_ascii()
                && !turn_metadata.chars().any(char::is_control)
            {
                extra_headers.push(("x-codex-turn-metadata".into(), turn_metadata.to_string()));
            }
        }
        let model = model.trim();
        if !model.is_empty() && !model.chars().any(char::is_control) {
            extra_headers.push(("x-codex-routing-hint".into(), identity.routing_hint(model)));
        }
    }
    Ok(WsDial {
        url,
        proxy: outbound::dial_proxy_for_client(proxy),
        authorization: header_string(headers, "authorization"),
        account_id: header_string(headers, "chatgpt-account-id"),
        extra_headers,
        preserve_client_identity: !identity.enabled,
    })
}

fn is_ws_passthrough_excluded(name: &HeaderName) -> bool {
    is_hop(name)
        || matches!(
            name.as_str().to_ascii_lowercase().as_str(),
            "authorization"
                | "chatgpt-account-id"
                | "sec-websocket-key"
                | "sec-websocket-version"
                | "sec-websocket-extensions"
                | "sec-websocket-protocol"
        )
}

fn header_string(headers: &HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| {
            !value.is_empty() && value.is_ascii() && !value.chars().any(char::is_control)
        })
        .map(str::to_owned)
}

fn merge_request_context_from_headers(context: &mut identity::RequestContext, headers: &HeaderMap) {
    if context.session_id.is_none() {
        context.session_id =
            header_value(headers, "session-id").or_else(|| header_value(headers, "session_id"));
    }
    if context.window_id.is_none() {
        context.window_id = header_value(headers, "x-codex-window-id")
            .or_else(|| header_value(headers, "window_id"));
    }
    if context.thread_id.is_none() {
        context.thread_id = header_value(headers, "thread-id")
            .or_else(|| header_value(headers, "x-client-request-id"));
    }
    if context.parent_thread_id.is_none() {
        context.parent_thread_id = header_value(headers, "x-codex-parent-thread-id");
    }
    if context.subagent.is_none() {
        context.subagent = header_value(headers, "x-openai-subagent");
    }
    if context.turn_metadata.is_none() {
        if let Some(value) = header_value(headers, "x-codex-turn-metadata") {
            if let Ok(serde_json::Value::Object(snapshot)) =
                serde_json::from_str::<serde_json::Value>(&value)
            {
                context.turn_metadata = Some(value);
                if context.thread_id.is_none() {
                    context.thread_id = snapshot
                        .get("thread_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                }
                if context.turn_id.is_none() {
                    context.turn_id = snapshot
                        .get("turn_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                }
                if context.parent_thread_id.is_none() {
                    context.parent_thread_id = snapshot
                        .get("parent_thread_id")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                }
                if context.subagent.is_none() {
                    context.subagent = snapshot
                        .get("subagent_kind")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                }
            }
        }
    }
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
    // Frames without session/window metadata still belong to this client
    // WebSocket. Keep one fallback scope for the socket instead of sharing the
    // process-wide runtime IDs with other windows.
    let socket_scope = format!("ws:{}", uuid::Uuid::new_v4());
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
        let mut header_context = identity::RequestContext::default();
        merge_request_context_from_headers(&mut header_context, &client_headers);
        let root_identity = app.vm_identity.lock().await.clone();
        let (frame, client_model, scoped_identity) = prepare_client_ws_frame(
            &app,
            &text,
            Some(&header_context),
            &root_identity,
            &socket_scope,
        )
        .await?;
        let model = frame
            .get("model")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let diag_req = diag::Request {
            id: diag::next_id(),
            flow: "business".into(),
            model: Some(logs::safe_text(&model, 80)).filter(|value| !value.is_empty()),
            route_kind: "websocket".into(),
            proxy_session: None,
        };
        diag::emit(
            "request",
            Some(&diag_req),
            json!({"method": "WS", "path": "/responses"}),
        );
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
        let mut request_context = identity::request_context_from_value(&frame);
        merge_request_context_from_headers(&mut request_context, &client_headers);
        let dial = match current_ws_dial(
            &app,
            &client_headers,
            &model,
            &request_context,
            &scoped_identity,
        )
        .await
        {
            Ok(dial) => dial,
            Err(err) => {
                let mut metrics = logs::ResponseBodyMetrics::new("");
                let error_message = err.to_string();
                finish_client_ws_turn(
                    &app,
                    account.as_ref(),
                    &model,
                    started,
                    billing,
                    &mut metrics,
                    true,
                    None,
                    Some(error_message.as_str()),
                    Some(&diag_req),
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
                let error_message = err.to_string();
                finish_client_ws_turn(
                    &app,
                    account.as_ref(),
                    &model,
                    started,
                    billing,
                    &mut metrics,
                    true,
                    None,
                    Some(error_message.as_str()),
                    Some(&diag_req),
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
        let mut failure_kind = None;
        let mut failure_message = None;
        while let Some(event) = rx.recv().await {
            match event {
                Ok(json) => {
                    // Native WS frames are already complete JSON events. Parse
                    // them before forwarding so a large terminal frame cannot
                    // be discarded by the bounded SSE inspector.
                    metrics.observe_ws_event(&json, started.elapsed().as_millis());
                    if let Err(error) = socket.send(WsMessage::text(json)).await {
                        failed = true;
                        failure_kind = Some("client_send");
                        failure_message = Some(error.to_string());
                        break;
                    }
                }
                Err(error) => {
                    failed = true;
                    failure_message = Some(error.to_string());
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
            failure_kind,
            failure_message.as_deref(),
            Some(&diag_req),
        )
        .await;
    }
}

/// The frame to send upstream, and the model the client asked for before a
/// forced model replaced it.
async fn prepare_client_ws_frame(
    app: &App,
    text: &str,
    request_context: Option<&identity::RequestContext>,
    root_identity: &VmIdentity,
    fallback_scope: &str,
) -> Result<(serde_json::Value, Option<String>, VmIdentity)> {
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
    let mut frame_context = identity::request_context_from_value(&frame);
    if let Some(request_context) = request_context {
        if frame_context.session_id.is_none() {
            frame_context.session_id = request_context.session_id.clone();
        }
        if frame_context.window_id.is_none() {
            frame_context.window_id = request_context.window_id.clone();
        }
        if frame_context.thread_id.is_none() {
            frame_context.thread_id = request_context.thread_id.clone();
        }
        if frame_context.turn_id.is_none() {
            frame_context.turn_id = request_context.turn_id.clone();
        }
        if frame_context.parent_thread_id.is_none() {
            frame_context.parent_thread_id = request_context.parent_thread_id.clone();
        }
        if frame_context.subagent.is_none() {
            frame_context.subagent = request_context.subagent.clone();
        }
        if frame_context.turn_metadata.is_none() {
            frame_context.turn_metadata = request_context.turn_metadata.clone();
        }
    }
    let identity = root_identity.scoped_for_request(&frame_context, fallback_scope);
    identity::rewrite_client_metadata_value_with_context(
        &mut frame,
        &identity,
        Some(&frame_context),
    );
    Ok((frame, client_model, identity))
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

async fn current_ws_dial(
    app: &App,
    client_headers: &HeaderMap,
    model: &str,
    request_context: &identity::RequestContext,
    identity: &VmIdentity,
) -> Result<WsDial> {
    business_ws_dial(app, client_headers, model, request_context, identity, false).await
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
    let identity = app.vm_identity.lock().await.clone();
    business_ws_dial(
        app,
        &HeaderMap::new(),
        &model,
        &identity::RequestContext::default(),
        &identity,
        true,
    )
    .await
}

async fn business_ws_dial(
    app: &App,
    client_headers: &HeaderMap,
    model: &str,
    request_context: &identity::RequestContext,
    identity: &VmIdentity,
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
    ws_dial(
        &settings.upstream,
        &proxy,
        &headers,
        &identity,
        model,
        request_context,
    )
}

async fn finish_client_ws_turn(
    app: &App,
    account: Option<&BillingAccount>,
    model: &str,
    started: Instant,
    billing: Option<BillingRequest>,
    metrics: &mut logs::ResponseBodyMetrics,
    failed: bool,
    failure_kind: Option<&str>,
    failure_message: Option<&str>,
    diag_req: Option<&diag::Request>,
) {
    // A protocol terminal event is a completed WebSocket exchange at the
    // transport layer. Keep its HTTP-compatible 200 status while preserving
    // the protocol error kind; only a broken socket is a 502 transport error.
    let error_kind = metrics
        .error_kind
        .map(str::to_owned)
        .or_else(|| failure_kind.map(str::to_owned))
        .or_else(|| failed.then(|| "ws_upstream".into()));
    let error_message = metrics
        .error_message()
        .map(str::to_owned)
        .or_else(|| failure_message.map(str::to_owned));
    let status = if failed { 502 } else { 200 };
    diag::emit(
        "ws_finish",
        diag_req,
        json!({
            "status": status,
            "failed": error_kind.is_some(),
            "error": error_kind,
            "errorMessage": error_message,
            "usageSeen": metrics.usage_seen(),
            "inputTokens": metrics.input_tokens(),
            "outputTokens": metrics.output_tokens(),
            "usageSource": metrics.usage_seen().then_some("provider_response"),
            "responseModel": metrics.upstream_response_model(),
            "events": metrics.sse_event_summary(),
        }),
    );
    if let Some(request) = billing {
        let usage_complete = metrics.usage_seen()
            && metrics.input_tokens().is_some()
            && metrics.output_tokens().is_some();
        request.settle(UsageOutcome {
            state: if error_kind.is_some() {
                UsageState::Interrupted
            } else if usage_complete {
                UsageState::Measured
            } else {
                UsageState::MissingUsage
            },
            finished_at: Some(chrono::Utc::now().to_rfc3339()),
            http_status: Some(status),
            response_model: metrics.upstream_response_model().map(str::to_owned),
            usage: metrics.token_usage(),
            usage_source: metrics.usage_seen().then(|| "provider_response".into()),
            error_kind: error_kind.clone(),
            error_message: error_message.clone(),
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
    details.error_kind = error_kind.clone();
    details.diag = diag_req.cloned();
    app.record("WS", "/responses", status, started, details)
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
        let expected_identity = identity.scoped_for_request(
            &identity::RequestContext {
                session_id: Some("client-session".into()),
                window_id: Some("client-window".into()),
                thread_id: Some("thread-keep".into()),
                ..Default::default()
            },
            "test-http",
        );
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
        assert_eq!(headers["session-id"], expected_identity.session_id);
        assert!(headers.get("session_id").is_none());
        assert_eq!(headers["x-codex-installation-id"], identity.installation_id);
        assert_eq!(headers["x-codex-window-id"], expected_identity.window_id);
        assert_eq!(headers["thread-id"], "thread-keep");
        assert_eq!(headers["x-client-request-id"], "thread-keep");
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
        assert_eq!(metadata["session_id"], expected_identity.session_id);
        assert_eq!(metadata["x-codex-window-id"], expected_identity.window_id);
        assert_eq!(metadata["thread_id"], "thread-keep");
        assert_eq!(metadata["turn_id"], "turn-keep");
        server.abort();
    }

    #[tokio::test]
    async fn business_forward_passes_client_identity_when_vm_is_disabled() {
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
        app.vm_identity.lock().await.enabled = false;
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let body = br#"{"model":"gpt-test","client_metadata":{"x-codex-installation-id":"client-install","session_id":"client-session","x-codex-window-id":"client-window","thread_id":"thread-keep"}}"#;
        let response = proxy_http(
            app,
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
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let (headers, forwarded) = received.recv().await.unwrap();
        assert_eq!(headers["user-agent"], "client-ua");
        assert_eq!(headers["originator"], "client-origin");
        assert_eq!(headers["version"], "9.9.9");
        assert_eq!(headers["session_id"], "client-session");
        assert_eq!(headers["x-codex-installation-id"], "client-install");
        assert_eq!(headers["x-codex-window-id"], "client-window");
        assert_eq!(headers["x-codex-routing-hint"], "model=client");
        assert_eq!(forwarded, body);
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
            &identity::RequestContext::default(),
        )
        .unwrap();
        assert!(!dial.preserve_client_identity);
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
        assert_eq!(extra("session-id"), identity.session_id);
        assert_eq!(extra("x-codex-window-id"), identity.window_id);
        assert_eq!(extra("thread-id"), identity.thread_id);
        assert_eq!(extra("x-client-request-id"), identity.thread_id);
        assert_eq!(extra("x-codex-routing-hint"), "model=gpt-test");
        assert!(!extra("user-agent").contains("client-ua"));
    }

    #[test]
    fn ws_dial_passes_client_identity_when_vm_is_disabled() {
        let mut identity = VmIdentity::ephemeral();
        identity.enabled = false;
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer secret".parse().unwrap());
        headers.insert("chatgpt-account-id", "acct".parse().unwrap());
        headers.insert("user-agent", "client-ua".parse().unwrap());
        headers.insert("originator", "client-origin".parse().unwrap());
        headers.insert("version", "9.9.9".parse().unwrap());
        headers.insert("x-codex-installation-id", "client-install".parse().unwrap());
        headers.insert("x-codex-routing-hint", "model=client".parse().unwrap());
        let dial = ws_dial(
            "https://chatgpt.com/backend-api/codex/responses",
            "",
            &headers,
            &identity,
            "gpt-test",
            &identity::RequestContext::default(),
        )
        .unwrap();
        let extra = |name: &str| {
            dial.extra_headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        assert!(dial.preserve_client_identity);
        assert_eq!(extra("user-agent"), Some("client-ua"));
        assert_eq!(extra("originator"), Some("client-origin"));
        assert_eq!(extra("version"), Some("9.9.9"));
        assert_eq!(extra("x-codex-installation-id"), Some("client-install"));
        assert_eq!(extra("x-codex-routing-hint"), Some("model=client"));
        assert!(extra("session-id").is_none());
    }

    #[tokio::test]
    async fn client_ws_frame_replaces_device_metadata() {
        let app = App::new(Settings {
            forced_model: "gpt-forced".into(),
            ..Settings::default()
        })
        .unwrap();
        let identity = app.vm_identity.lock().await.clone();
        let root_identity = app.vm_identity.lock().await.clone();
        let (frame, client_model, _) = prepare_client_ws_frame(
            &app,
            r#"{"type":"response.create","model":"gpt-test","client_metadata":{"x-codex-installation-id":"client","session_id":"client","x-codex-window-id":"client","thread_id":"keep","turn_id":"turn"}}"#,
            None,
            &root_identity,
            "test-ws",
        )
        .await
        .unwrap();
        let expected_identity = identity.scoped_for_request(
            &identity::RequestContext {
                session_id: Some("client".into()),
                window_id: Some("client".into()),
                thread_id: Some("keep".into()),
                turn_id: Some("turn".into()),
                ..Default::default()
            },
            "test-ws",
        );
        assert_eq!(
            frame["client_metadata"]["x-codex-installation-id"],
            identity.installation_id
        );
        assert_eq!(
            frame["client_metadata"]["session_id"],
            expected_identity.session_id
        );
        assert_eq!(
            frame["client_metadata"]["x-codex-window-id"],
            expected_identity.window_id
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

    #[tokio::test]
    async fn websocket_protocol_errors_preserve_200_and_bill_as_interrupted() {
        for event_type in ["error", "response.failed", "response.incomplete"] {
            let app = Arc::new(App::new(Settings::default()).unwrap());
            let account = BillingAccount {
                id: "account-protocol".into(),
                email: Some("protocol@example.com".into()),
            };
            let billing = app
                .begin_ws_billing(
                    Instant::now(),
                    &account,
                    Some("gpt-test"),
                    Some("gpt-test"),
                    None,
                )
                .unwrap();
            let request_id = billing.request_id.clone();
            let mut metrics = logs::ResponseBodyMetrics::new("");
            let event = match event_type {
                "error" => r#"data: {"type":"error","error":{"message":"provider rejected"}}

"#
                .to_string(),
                "response.failed" => {
                    r#"data: {"type":"response.failed","error":{"code":"rate_limit","message":"try later"}}

"#
                        .to_string()
                }
                _ => {
                    r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"timeout"}}}

"#
                        .to_string()
                }
            };
            metrics.observe(event.as_bytes(), 1, true);
            metrics.finish(2);

            finish_client_ws_turn(
                &app,
                Some(&account),
                "gpt-test",
                Instant::now(),
                Some(billing),
                &mut metrics,
                false,
                None,
                None,
                None,
            )
            .await;

            let record = app.billing.get_by_id(&request_id).unwrap().unwrap();
            let expected_error = if event_type == "response.incomplete" {
                "response_incomplete"
            } else {
                "response_failed"
            };
            let expected_message = match event_type {
                "error" => Some("message=provider rejected"),
                "response.failed" => Some("code=rate_limit; message=try later"),
                _ => Some("reason=timeout"),
            };
            assert_eq!(record.state, UsageState::Interrupted, "{event_type}");
            assert_eq!(record.http_status, Some(200), "{event_type}");
            assert_eq!(
                record.error_kind.as_deref(),
                Some(expected_error),
                "{event_type}"
            );
            assert_eq!(
                record.error_message.as_deref(),
                expected_message,
                "{event_type}"
            );
            let entry = app.logs.lock().await.back().unwrap().snapshot();
            assert_eq!(entry.status, 200, "{event_type}");
            assert_eq!(
                entry.error_kind.as_deref(),
                Some(expected_error),
                "{event_type}"
            );
        }
    }

    #[tokio::test]
    async fn websocket_transport_failure_remains_502_and_ws_upstream() {
        let app = Arc::new(App::new(Settings::default()).unwrap());
        let account = BillingAccount {
            id: "account-transport".into(),
            email: None,
        };
        let billing = app
            .begin_ws_billing(
                Instant::now(),
                &account,
                Some("gpt-test"),
                Some("gpt-test"),
                None,
            )
            .unwrap();
        let request_id = billing.request_id.clone();
        let mut metrics = logs::ResponseBodyMetrics::new("");
        finish_client_ws_turn(
            &app,
            Some(&account),
            "gpt-test",
            Instant::now(),
            Some(billing),
            &mut metrics,
            true,
            None,
            None,
            None,
        )
        .await;

        let record = app.billing.get_by_id(&request_id).unwrap().unwrap();
        assert_eq!(record.state, UsageState::Interrupted);
        assert_eq!(record.http_status, Some(502));
        assert_eq!(record.error_kind.as_deref(), Some("ws_upstream"));
        let entry = app.logs.lock().await.back().unwrap().snapshot();
        assert_eq!(entry.status, 502);
        assert_eq!(entry.error_kind.as_deref(), Some("ws_upstream"));
    }
}
