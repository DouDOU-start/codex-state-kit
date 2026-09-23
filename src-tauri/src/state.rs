use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use codex_state_kit::browser_login::BrowserLogin;
use codex_state_kit::mihomo::{MihomoPaths, MihomoRuntime};
use codex_state_kit::{load_settings, App, PendingLogin, ProxyHandle};

#[derive(Clone)]
pub enum LoginSession {
    Device(PendingLogin),
    Browser(Arc<BrowserLogin>),
}

#[derive(Default)]
pub struct LoginSlot {
    pub generation: u64,
    pub pending: Option<LoginSession>,
    pub closed: bool,
}

impl LoginSlot {
    pub fn cancel(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(pending) = self.pending.take() {
            match pending {
                LoginSession::Device(pending) => pending.cancel(),
                LoginSession::Browser(pending) => pending.cancel(),
            }
        }
    }
}

pub struct AppState {
    pub proxy: ProxyHandle,
    pub pending_login: Mutex<LoginSlot>,
    restored: AtomicBool,
    update_prepared: AtomicBool,
    supervisors: Mutex<Vec<tauri::async_runtime::JoinHandle<()>>>,
}

impl AppState {
    pub fn initialize(mihomo_paths: MihomoPaths) -> Result<Self> {
        let settings = load_settings();
        let app = Arc::new(App::with_mihomo(
            settings,
            MihomoRuntime::new(Some(mihomo_paths)),
        )?);
        let proxy = ProxyHandle::new(app);
        proxy.enable_auto_attach();
        Ok(Self {
            proxy,
            pending_login: Mutex::new(LoginSlot::default()),
            restored: AtomicBool::new(false),
            update_prepared: AtomicBool::new(false),
            supervisors: Mutex::new(Vec::new()),
        })
    }

    pub fn core(&self) -> Arc<App> {
        self.proxy.app()
    }

    pub fn start_runtime(&self) {
        let mut tasks = self.supervisors.lock().expect("supervisors");
        let proxy = self.proxy.clone();
        tasks.push(tauri::async_runtime::spawn(async move {
            if let Err(err) = proxy.start_managed().await {
                eprintln!("failed to start proxy: {err:#}");
            }
        }));
        let proxy = self.proxy.clone();
        tasks.push(tauri::async_runtime::spawn(async move {
            proxy.run_attachment_supervisor().await;
        }));
        let proxy = self.proxy.clone();
        tasks.push(tauri::async_runtime::spawn(async move {
            proxy.run_sidecar_supervisor().await;
        }));
    }

    fn stop_supervisors(&self) {
        for task in self.supervisors.lock().expect("supervisors").drain(..) {
            task.abort();
        }
    }

    pub async fn prepare_update(&self) -> Result<()> {
        if self.update_prepared.swap(true, Ordering::SeqCst) {
            anyhow::bail!("更新安装已在进行中");
        }
        self.stop_supervisors();
        self.pending_login.lock().expect("pending login").cancel();
        if let Err(err) = self.proxy.restore_managed_routes() {
            self.resume_after_update_failure();
            return Err(err);
        }
        // Await cleanup before the Windows updater exits the process directly.
        self.proxy.app().mihomo.stop().await;
        self.proxy.stop().await;
        self.restored.store(true, Ordering::SeqCst);
        Ok(())
    }

    pub fn resume_after_update_failure(&self) {
        if !self.update_prepared.swap(false, Ordering::SeqCst) {
            return;
        }
        self.restored.store(false, Ordering::SeqCst);
        self.proxy.enable_auto_attach();
        self.start_runtime();
    }

    pub fn update_is_prepared(&self) -> bool {
        self.update_prepared.load(Ordering::SeqCst) && self.restored.load(Ordering::SeqCst)
    }

    pub fn restore_once(&self) {
        if self.restored.swap(true, Ordering::SeqCst) {
            return;
        }
        self.stop_supervisors();
        {
            let mut login = self.pending_login.lock().expect("pending login");
            login.closed = true;
            login.cancel();
        }
        if let Err(err) = self.proxy.restore_managed_routes() {
            eprintln!("restore on exit failed: {err:#}");
        }
        let proxy = self.proxy.clone();
        tauri::async_runtime::spawn(async move {
            proxy.app().mihomo.stop().await;
            proxy.stop().await;
        });
    }
}
