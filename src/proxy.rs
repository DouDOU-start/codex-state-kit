use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode, Uri, Version};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex, Notify};
use tokio::task::JoinHandle;
use url::Url;

use crate::attach::{self, is_attached};
use crate::fetch;
use crate::login::{self, has_chatgpt_login};
use crate::logs::{self, LogEntry, NetworkLogDetails};
use crate::settings::{save_settings, OutboundMode, Settings, SettingsPatch};
use crate::turn_state::{self, TurnStateStore, TurnStateView};
use crate::warp::{WarpRuntime, WarpStatus};

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
    pub upstream_proxy: String,
    pub outbound_mode: OutboundMode,
    pub warp_http2: bool,
    pub warp: WarpStatus,
    pub fetch_error: Option<String>,
    pub fetch_ok_at: Option<String>,
    pub turn_state: TurnStateView,
    pub degraded: bool,
    pub degraded_at: Option<String>,
    pub logs: Vec<LogEntry>,
}

pub struct App {
    pub warp: WarpRuntime,
    pub settings: Mutex<Settings>,
    pub logs: Mutex<VecDeque<LogEntry>>,
    pub proxy_ok: AtomicBool,
    pub login_http: reqwest::Client,
    leftover_restored: AtomicBool,
    proxy_error: Mutex<Option<String>>,
    fetch_error: Mutex<Option<String>>,
    fetch_ok_at: Mutex<Option<String>>,
    fetch_round: AtomicU32,
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
}

impl App {
    pub fn new(settings: Settings) -> Result<Self> {
        Self::with_warp(settings, WarpRuntime::default())
    }

    pub fn with_warp(settings: Settings, warp: WarpRuntime) -> Result<Self> {
        let http = upstream_http_client(&settings.upstream_proxy)?;
        Ok(Self {
            warp,
            settings: Mutex::new(settings),
            logs: Mutex::new(VecDeque::with_capacity(80)),
            proxy_ok: AtomicBool::new(false),
            login_http: crate::login::http_client()?,
            leftover_restored: AtomicBool::new(false),
            proxy_error: Mutex::new(None),
            fetch_error: Mutex::new(None),
            fetch_ok_at: Mutex::new(None),
            fetch_round: AtomicU32::new(0),
            turn_state: Mutex::new(TurnStateStore::load()),
            http: Mutex::new(http),
            degraded: AtomicBool::new(false),
            degraded_at: Mutex::new(None),
            degrade_notify: Notify::new(),
            warp_wake: Notify::new(),
            model_notify: Notify::new(),
            seeds_registered: AtomicBool::new(false),
        })
    }

    async fn sync_logged_in_account(&self) {
        let home = self.settings.lock().await.codex_home.clone();
        let Ok(creds) = login::chatgpt_credentials(Path::new(&home)) else {
            return;
        };
        let changed = self.turn_state.lock().await.bind_account(&creds.account_id);
        if changed {
            self.seeds_registered.store(false, Ordering::Relaxed);
            *self.fetch_error.lock().await = None;
            self.model_notify.notify_one();
        }
    }

    pub async fn status(&self) -> Status {
        self.sync_logged_in_account().await;
        let settings = self.settings.lock().await.clone();
        let logs = self.logs.lock().await.iter().cloned().collect();
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
            fetch_error: self.fetch_error.lock().await.clone(),
            fetch_ok_at: self.fetch_ok_at.lock().await.clone(),
            turn_state: self.turn_state.lock().await.view(),
            degraded: self.degraded.load(Ordering::Relaxed),
            degraded_at: self.degraded_at.lock().await.clone(),
            logs,
        }
    }

    pub async fn refresh_turn_state(&self) -> Result<Status> {
        let settings = self.settings.lock().await.clone();
        let mut last_error: Option<anyhow::Error> = None;
        for model in &settings.models {
            if let Err(e) = self.fetch_once(&settings, model).await {
                eprintln!("[refresh] 模型 {} 获取失败: {e:#}", model);
                last_error = Some(e);
            }
        }
        if let Some(e) = last_error {
            if settings.models.len() == 1 {
                return Err(e);
            }
        }
        Ok(self.status().await)
    }

    /// 用户切换绑定的 token 长度（传 None 恢复账号自动识别的 292/332）
    pub async fn set_bound_token_len(&self, len: Option<usize>) -> Status {
        {
            let mut store = self.turn_state.lock().await;
            store.set_bound_len(len);
        }
        self.degrade_notify.notify_one();
        self.status().await
    }

    pub async fn set_model_bound_token_len(&self, model: &str, len: Option<usize>) -> Status {
        {
            let mut store = self.turn_state.lock().await;
            store.set_model_bound_len(model, len);
        }
        self.degrade_notify.notify_one();
        self.status().await
    }

    fn fetch_settings(&self, settings: &Settings) -> Result<Settings> {
        let mut effective = settings.clone();
        if effective.outbound_mode == OutboundMode::Warp {
            effective.outbound_proxy = self.warp.proxy_url()?;
        }
        if effective.outbound_proxy.trim().is_empty() {
            anyhow::bail!("尚未配置出站代理");
        }
        Ok(effective)
    }

    async fn fetch_once(&self, settings: &Settings, model: &str) -> Result<String> {
        let effective = self.fetch_settings(settings).map_err(|err| err.to_string());
        let settings = match effective {
            Ok(settings) => settings,
            Err(message) => {
                *self.fetch_error.lock().await = Some(message.clone());
                anyhow::bail!("{message}");
            }
        };
        if !has_chatgpt_login(Path::new(&settings.codex_home)) {
            let message = "尚未登录 ChatGPT".to_string();
            *self.fetch_error.lock().await = Some(message.clone());
            anyhow::bail!("{message}");
        }
        let creds = match login::chatgpt_credentials(Path::new(&settings.codex_home)) {
            Ok(creds) => creds,
            Err(err) => {
                let message = format!("{err:#}");
                *self.fetch_error.lock().await = Some(message.clone());
                anyhow::bail!("{message}");
            }
        };
        let client = match fetch::http_client(&settings.outbound_proxy) {
            Ok(client) => client,
            Err(err) => {
                let message = format!("{err:#}");
                *self.fetch_error.lock().await = Some(message.clone());
                anyhow::bail!("{message}");
            }
        };
        let started = Instant::now();
        let mut details = NetworkLogDetails::default();
        let token =
            match fetch::fetch_turn_state_with_log(&client, &settings, &creds, model, &mut details)
                .await
            {
                Ok(token) => token,
                Err(err) => {
                    self.record_fetch(started, details).await;
                    let message = format!("{err:#}");
                    eprintln!("[{}] turn-state fetch failed: {message}", model);
                    *self.fetch_error.lock().await = Some(message.clone());
                    anyhow::bail!("{message}");
                }
            };
        if turn_state::TurnState::from_token(&token, "fetch").is_none() {
            details.turn_state_action = "rejected_invalid".into();
            self.record_fetch(started, details).await;
            let message = format!("[{}] 上游 token 无法解析", model);
            *self.fetch_error.lock().await = Some(message.clone());
            anyhow::bail!("{message}");
        }
        if turn_state::is_degraded_token(&token) {
            details.turn_state_action = "rejected_degraded".into();
            self.record_fetch(started, details).await;
            let message = format!(
                "[{}] 采到 312 token（{}字节），已丢弃，等待重试",
                model,
                token.len()
            );
            eprintln!("⚠ {message}");
            *self.fetch_error.lock().await = Some(message.clone());
            anyhow::bail!("{message}");
        }
        details.turn_state_action = "captured".into();
        self.record_fetch(started, details).await;
        self.turn_state.lock().await.capture(model, &token, "fetch");
        *self.fetch_error.lock().await = None;
        *self.fetch_ok_at.lock().await =
            Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        Ok(token)
    }

    async fn refresh_if_needed(&self) -> Duration {
        let saved = self.settings.lock().await.clone();
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
        }

        // 312 降智信号 → 清池（所有模型的 token，但保留追踪）
        if self.degraded.swap(false, Ordering::Relaxed) {
            eprintln!("312 降智 / 服务端拒绝信号，清池重打 292（所有模型）");
            self.turn_state.lock().await.invalidate_all();
            *self.degraded_at.lock().await = None;
        }

        // 获取所有活跃模型（最近 60 分钟内有请求的），检查哪些需要刷新
        let models_needing_refresh: Vec<String> = {
            let store = self.turn_state.lock().await;
            store
                .all_active_models()
                .into_iter()
                .filter(|m| store.needs_refresh(m))
                .collect()
        };

        if models_needing_refresh.is_empty() {
            return fetch::CHECK_INTERVAL;
        }

        // 逐模型并发获取，每个模型 10 路并发
        const CONCURRENCY: usize = 10;
        self.fetch_round
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let round = self.fetch_round.load(std::sync::atomic::Ordering::Relaxed);

        let mut all_ok = true;

        for model in &models_needing_refresh {
            eprintln!(
                "[{}] 需要新 token，第 {} 轮并发 {} 路获取...",
                model, round, CONCURRENCY
            );
            let bound_label = self.turn_state.lock().await.bound_len();
            *self.fetch_error.lock().await = Some(format!(
                "正在获取 {} 的 {} Token（第 {} 轮）…",
                model, bound_label, round
            ));

            let bound_len = self.turn_state.lock().await.bound_len_for(model);

            let mut handles = tokio::task::JoinSet::new();
            for _ in 0..CONCURRENCY {
                let s = settings.clone();
                let rotated = s.outbound_proxy.clone();
                let client = match fetch::http_client(&rotated) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let creds = match login::chatgpt_credentials(Path::new(&s.codex_home)) {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let m = model.clone();
                handles.spawn(async move {
                    let started = Instant::now();
                    let mut details = NetworkLogDetails::default();
                    let result =
                        fetch::fetch_turn_state_with_log(&client, &s, &creds, &m, &mut details)
                            .await;
                    (started, result, details)
                });
            }

            // 收集所有返回的 token
            let mut all_tokens: Vec<String> = Vec::new();
            while let Some(result) = handles.join_next().await {
                if let Ok((started, result, mut details)) = result {
                    if let Ok(token) = result {
                        if turn_state::TurnState::from_token(&token, "fetch").is_some() {
                            details.turn_state_action = "received".into();
                            all_tokens.push(token);
                        } else {
                            details.turn_state_action = "rejected_invalid".into();
                        }
                    }
                    self.record_fetch(started, details).await;
                }
            }

            // 统计 token 长度分布
            let mut dist_map: std::collections::HashMap<usize, u32> =
                std::collections::HashMap::new();
            for token in &all_tokens {
                *dist_map.entry(token.trim().len()).or_default() += 1;
            }
            let mut distribution: Vec<turn_state::TokenLenCount> = dist_map
                .into_iter()
                .map(|(len, count)| turn_state::TokenLenCount { len, count })
                .collect();
            distribution.sort_by_key(|d| d.len);

            // 记录分布（不持久化，仅内存展示）
            {
                let mut store = self.turn_state.lock().await;
                store.record_distribution(model, distribution.clone());
            }

            // 日志：展示分布
            let dist_str: String = distribution
                .iter()
                .map(|d| format!("{}×{}", d.len, d.count))
                .collect::<Vec<_>>()
                .join(", ");
            debug_log(&format!(
                "[{}] 第 {} 轮获取完成，共 {} 个 token，分布: [{}]，绑定长度: {}",
                model,
                round,
                all_tokens.len(),
                dist_str,
                bound_len
            ));

            // 全量入池（所有长度都缓存，方便切换绑定时不用重新获取）
            {
                let mut store = self.turn_state.lock().await;
                let matched = store.capture_batch(model, &all_tokens, "fetch");
                if matched > 0 {
                    debug_log(&format!(
                        "✅ [{}] 全部 {} 个 token 入池，其中 {} 个匹配绑定长度 {}",
                        model, all_tokens.len(), matched, bound_len
                    ));
                } else {
                    debug_log(&format!(
                        "⚠ [{}] 全部 {} 个 token 入池，但无匹配绑定长度 {} 的",
                        model, all_tokens.len(), bound_len
                    ));
                    all_ok = false;
                }
            }
        }

        if all_ok {
            *self.fetch_error.lock().await = None;
            *self.fetch_ok_at.lock().await = Some(
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            );
            fetch::CHECK_INTERVAL
        } else {
            Duration::ZERO
        }
    }

    pub(crate) fn signal_degradation(&self) {
        self.degraded.store(true, Ordering::Relaxed);
        self.degrade_notify.notify_one();
    }

    async fn record(
        &self,
        method: &str,
        path: &str,
        status: u16,
        started: Instant,
        details: NetworkLogDetails,
    ) {
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
        let next_http = if old.upstream_proxy != next.upstream_proxy {
            Some(upstream_http_client(&next.upstream_proxy)?)
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
            || old.codex_home != next.codex_home;
        if route_changed {
            self.stop_fetch_loop().await;
            self.app.turn_state.lock().await.invalidate_all();
            *self.app.fetch_error.lock().await = None;
            *self.app.fetch_ok_at.lock().await = None;
            self.app.degraded.store(false, Ordering::Relaxed);
            *self.app.degraded_at.lock().await = None;
            if next.outbound_mode == OutboundMode::Manual || old.warp_http2 != next.warp_http2 {
                self.app.warp.stop().await;
            }
        }
        {
            let mut settings = self.app.settings.lock().await;
            *settings = next.clone();
            if let Some(http) = next_http {
                *self.app.http.lock().await = http;
            }
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
        } else if route_changed {
            self.start_fetch_loop().await;
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
            }
            tokio::select! {
                _ = tokio::time::sleep(wait) => {},
                _ = self.app.warp_wake.notified() => {},
            }
        }
    }

    pub async fn connect_warp(&self, accept_terms: bool) -> Result<Status> {
        let _change = self.settings_change.lock().await;
        let settings = self.app.settings.lock().await.clone();
        if settings.outbound_mode != OutboundMode::Warp {
            anyhow::bail!("请先选择内置 WARP 模式");
        }
        self.stop_fetch_loop().await;
        let result = self
            .app
            .warp
            .connect(accept_terms, settings.warp_http2)
            .await;
        self.start_fetch_loop().await;
        result?;
        Ok(self.app.status().await)
    }

    pub async fn stop_warp(&self) -> Status {
        let _change = self.settings_change.lock().await;
        self.stop_fetch_loop().await;
        self.app.warp.stop().await;
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
    match forward_http_with_log(&app, req, &mut details).await {
        Ok(resp) => {
            app.record(
                method.as_str(),
                &path,
                resp.status().as_u16(),
                started,
                details,
            )
            .await;
            resp
        }
        Err(err) => {
            app.record(method.as_str(), &path, 502, started, details)
                .await;
            (StatusCode::BAD_GATEWAY, err.to_string()).into_response()
        }
    }
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
    forward_http_with_log(app, req, &mut details).await
}

async fn forward_http_with_log(
    app: &App,
    req: Request<Body>,
    details: &mut NetworkLogDetails,
) -> Result<Response> {
    let (upstream, home, upstream_proxy, http) = {
        let settings = app.settings.lock().await;
        (
            settings.upstream.clone(),
            settings.codex_home.clone(),
            settings.upstream_proxy.clone(),
            app.http.lock().await.clone(),
        )
    };
    let effective_proxy = fetch::outbound_proxy_for_client(&upstream_proxy);
    *details = logs::network_details(&upstream, &effective_proxy);
    let (mut parts, body) = req.into_parts();
    let target = join_upstream(&upstream, &parts.uri)?;
    let path = parts.uri.path();

    // 先读取 body，以便从中提取 model 字段
    let bytes = axum::body::to_bytes(body, 32 * 1024 * 1024)
        .await
        .context("read body")?;
    details.body_bytes = bytes.len();

    let should_stamp = turn_state::should_stamp_http(parts.method.as_str(), path);
    let content_encoding = parts
        .headers
        .get("content-encoding")
        .and_then(|v| v.to_str().ok());
    details.content_encoding = logs::safe_content_encoding(content_encoding);
    details.transport = if parts
        .headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/event-stream"))
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

    let mut injected_token: Option<String> = None;
    if should_stamp {
        let request_model = turn_state::extract_model_from_body(&bytes);
        details.model = request_model
            .as_deref()
            .map(|model| logs::safe_text(model, 80))
            .filter(|model| !model.is_empty());
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

        let client_already_has = turn_state::has_http_turn_state(&parts.headers);
        if client_already_has {
            let store = app.turn_state.lock().await;
            // 严格按模型取 token — 不同模型的 token 不可混用
            let token = if let Some(ref model) = request_model {
                store.peek_for_model(model)
            } else {
                // 无法识别模型时不注入，保留客户端原 token
                None
            };
            if let Some(token) = token {
                turn_state::apply_http_header(&mut parts.headers, &token);
                details.turn_state_action = "replaced".into();
                details.turn_state_len = Some(token.len());
                eprintln!(
                    "[stamp] 替换 turn_state → token len={} model={:?} 到 {} {}",
                    token.len(),
                    request_model,
                    parts.method,
                    path
                );
                injected_token = Some(token);
            } else {
                details.turn_state_action = if request_model.is_some() {
                    "preserved_no_ticket".into()
                } else {
                    "preserved_unknown_model".into()
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
                "[stamp] 首次请求，不注入 turn_state（等服务端下发） {} {} model={:?}",
                parts.method, path, request_model
            );
        }
    } else {
        details.turn_state_action = "not_applicable".into();
    }
    login::apply_kit_auth_headers(&mut parts.headers, Path::new(&home));
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
    let upstream_resp = match builder.send().await {
        Ok(response) => response,
        Err(error) => {
            details.error_kind = Some(logs::request_error_kind(&error));
            return Err(error).context("upstream http");
        }
    };
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
    let stream = upstream_resp.bytes_stream();
    let body = Body::from_stream(stream);
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
mod tests {
    use super::*;

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
}
