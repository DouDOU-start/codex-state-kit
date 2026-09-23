//! App-owned, userspace WARP sidecar. Never changes system routes or proxy settings.
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone)]
pub struct WarpPaths {
    pub binary: PathBuf,
    pub data_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WarpStatus {
    pub available: bool,
    pub registered: bool,
    pub phase: String,
    pub proxy_url: Option<String>,
    pub exit_ip: Option<String>,
    pub country: Option<String>,
    pub error: Option<String>,
}

struct Inner {
    view: WarpStatus,
    child: Option<OwnedChild>,
    // Authentication stays in the backend, never serialized to the UI or settings.
    endpoint: Option<String>,
}

pub struct WarpRuntime {
    paths: Option<WarpPaths>,
    inner: Mutex<Inner>,
    operation: tokio::sync::Mutex<()>,
    cancel: tokio::sync::watch::Sender<u64>,
}

impl Default for WarpRuntime {
    fn default() -> Self {
        Self::new(None)
    }
}

impl WarpRuntime {
    pub fn new(paths: Option<WarpPaths>) -> Self {
        let available = paths.as_ref().is_some_and(|p| p.binary.is_file());
        let registered = paths
            .as_ref()
            .is_some_and(|p| p.data_dir.join("config.json").is_file());
        Self {
            paths,
            inner: Mutex::new(Inner {
                view: WarpStatus {
                    available,
                    registered,
                    phase: "stopped".into(),
                    proxy_url: None,
                    exit_ip: None,
                    country: None,
                    error: None,
                },
                child: None,
                endpoint: None,
            }),
            operation: tokio::sync::Mutex::new(()),
            cancel: tokio::sync::watch::channel(0).0,
        }
    }

    pub fn status(&self) -> WarpStatus {
        let mut inner = self.inner.lock().expect("warp state");
        let exited = inner
            .child
            .as_mut()
            .is_some_and(|child| !matches!(child.child.try_wait(), Ok(None)));
        if exited {
            inner.child.take();
            inner.endpoint = None;
            inner.view.phase = "error".into();
            inner.view.proxy_url = None;
            inner.view.exit_ip = None;
            inner.view.country = None;
            inner.view.error = Some("WARP 内核已退出，请重新连接。".into());
        }
        inner.view.clone()
    }

    pub fn proxy_url(&self) -> Result<String> {
        let view = self.status();
        if let Some(endpoint) = self.inner.lock().expect("warp state").endpoint.clone() {
            return Ok(endpoint);
        }
        bail!(
            "{}",
            view.error
                .unwrap_or_else(|| "内置 WARP 正在自动连接，请稍候。".into())
        )
    }

    pub async fn connect(&self, accept_terms: bool, http2: bool) -> Result<WarpStatus> {
        let mut cancellation = self.cancel.subscribe();
        let _operation = self
            .operation
            .try_lock()
            .context("WARP 正在连接或停止，请稍候")?;
        self.stop_inner();
        {
            let mut inner = self.inner.lock().expect("warp state");
            inner.view.phase = "starting".into();
            inner.view.error = None;
        }
        let start = async {
            let result = self.start(accept_terms, http2).await;
            if result.is_err() && !http2 && self.status().registered {
                // QUIC can be blocked by the network. Retry over TCP without UI setup.
                self.stop_inner();
                self.inner.lock().expect("warp state").view.phase = "connecting".into();
                self.start(false, true).await
            } else {
                result
            }
        };
        let result = tokio::select! {
            result = start => result,
            _ = cancellation.changed() => Err(anyhow::anyhow!("WARP 连接已取消")),
        };
        if let Err(err) = result {
            self.stop_inner();
            let message = format!("{err:#}");
            let mut inner = self.inner.lock().expect("warp state");
            inner.view.phase = "error".into();
            inner.view.error = Some(message.clone());
            bail!("{message}");
        }
        Ok(self.status())
    }

    async fn start(&self, accept_terms: bool, http2: bool) -> Result<()> {
        let paths = self
            .paths
            .as_ref()
            .context("此构建未包含 WARP 内核，请使用完整安装包")?;
        if !paths.binary.is_file() {
            bail!("WARP 内核文件缺失，请重新安装完整安装包");
        }
        let config = paths.data_dir.join("config.json");
        if !config.is_file() && !accept_terms {
            bail!("首次连接需同意 Cloudflare 服务条款，再初始化 WARP。");
        }
        fs::create_dir_all(&paths.data_dir).context("无法创建 WARP 数据目录")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&paths.data_dir, fs::Permissions::from_mode(0o700))?;
        }
        let log = File::create(paths.data_dir.join("warp.log")).context("无法写入 WARP 日志")?;
        if !config.is_file() {
            self.inner.lock().expect("warp state").view.phase = "registering".into();
            let temp = paths.data_dir.join("registration.json");
            // A previous failed registration must not cause an interactive overwrite prompt.
            if temp.exists() {
                fs::remove_file(&temp).context("清理未完成的 WARP 注册")?;
            }
            let mut command = sidecar_command(&paths.binary, &log)?;
            command.arg("--config").arg(&temp).args([
                "register",
                "--accept-tos",
                "--name",
                "Codex State Kit",
            ]);
            let mut child = OwnedChild::spawn(&mut command)?;
            let deadline = Instant::now() + Duration::from_secs(45);
            loop {
                if let Some(exit) = child.child.try_wait()? {
                    if !exit.success() {
                        bail!("WARP 注册失败，请检查网络，稍后重试。详情见应用数据目录 warp/warp.log。");
                    }
                    break;
                }
                if Instant::now() >= deadline {
                    bail!("WARP 注册超时，请检查网络后重试。");
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            validate_config(&temp)?;
            fs::rename(&temp, &config).context("保存 WARP 身份失败")?;
            self.inner.lock().expect("warp state").view.registered = true;
        }
        validate_config(&config)?;
        // Use a private dynamic port and per-process SOCKS credentials. A port race
        // cannot redirect requests into another local proxy that lacks the secret.
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let password = format!("{:032x}", rand::random::<u128>());
        let endpoint = format!("socks5h://statekit:{password}@127.0.0.1:{port}");
        let mut command = sidecar_command(&paths.binary, &log)?;
        command.arg("--config").arg(&config).args([
            "socks",
            "--bind",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--username",
            "statekit",
            "--password",
            &password,
            "--dns",
            "1.1.1.1",
            "--always-reconnect",
            "--reconnect-delay",
            "3s",
        ]);
        if http2 {
            command.arg("--http2");
        }
        drop(listener);
        let child = OwnedChild::spawn(&mut command)?;
        {
            let mut inner = self.inner.lock().expect("warp state");
            inner.child = Some(child);
            inner.view.phase = "connecting".into();
        }
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(&endpoint)?)
            .timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.status().phase == "error" {
                bail!("WARP 内核启动失败，请查看应用数据目录 warp/warp.log。");
            }
            // Listening alone is not a connected tunnel. Verify the actual route.
            if let Some((ip, country)) = probe_warp_exit(&client).await {
                let mut inner = self.inner.lock().expect("warp state");
                inner.endpoint = Some(endpoint);
                inner.view.phase = "connected".into();
                inner.view.proxy_url = Some(format!("socks5h://127.0.0.1:{port}"));
                inner.view.exit_ip = Some(ip);
                inner.view.country = country;
                inner.view.error = None;
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("WARP 隧道未通过连通性检查。可切换 TCP 兼容模式重试，或使用手动代理。");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn stop(&self) -> WarpStatus {
        self.cancel_connect();
        let _operation = self.operation.lock().await;
        self.stop_inner();
        self.status()
    }

    pub fn cancel_connect(&self) {
        self.cancel
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    pub async fn check_health(&self) {
        let endpoint = self.inner.lock().expect("warp state").endpoint.clone();
        let Some(endpoint) = endpoint else {
            return;
        };
        let probe: Result<(String, Option<String>)> = async {
            let client = reqwest::Client::builder()
                .proxy(reqwest::Proxy::all(&endpoint)?)
                .timeout(Duration::from_secs(8))
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            probe_warp_exit(&client)
                .await
                .context("WARP 出口未通过校验")
        }
        .await;
        let mut inner = self.inner.lock().expect("warp state");
        if inner.endpoint.as_deref() != Some(endpoint.as_str()) {
            return;
        }
        match probe {
            Ok((ip, country)) => {
                inner.view.phase = "connected".into();
                inner.view.exit_ip = Some(ip);
                inner.view.country = country;
                inner.view.error = None;
            }
            Err(_) => {
                inner.view.phase = "reconnecting".into();
                inner.view.exit_ip = None;
                inner.view.country = None;
                inner.view.error = Some("WARP 出口暂不可用，内核正在自动重连。".into());
            }
        }
    }

    fn stop_inner(&self) {
        let mut inner = self.inner.lock().expect("warp state");
        inner.child.take();
        inner.endpoint = None;
        inner.view.phase = "stopped".into();
        inner.view.proxy_url = None;
        inner.view.exit_ip = None;
        inner.view.country = None;
        inner.view.error = None;
    }
}

fn validate_config(path: &Path) -> Result<()> {
    let config: serde_json::Value =
        serde_json::from_slice(&fs::read(path).context("WARP 身份文件缺失")?)
            .context("WARP 身份文件损坏，请检查应用数据目录 warp/config.json")?;
    for key in ["private_key", "endpoint_v4", "endpoint_pub_key", "ipv4"] {
        if config
            .get(key)
            .and_then(|v| v.as_str())
            .is_none_or(str::is_empty)
        {
            bail!("WARP 身份文件不完整，缺少 {key}，请检查应用数据目录 warp/config.json");
        }
    }
    Ok(())
}

async fn probe_warp_exit(client: &reqwest::Client) -> Option<(String, Option<String>)> {
    for url in [
        "https://1.1.1.1/cdn-cgi/trace",
        "https://www.cloudflare.com/cdn-cgi/trace",
    ] {
        let Ok(response) = client.get(url).send().await else {
            continue;
        };
        if !response.status().is_success() {
            continue;
        }
        let Ok(body) = response.text().await else {
            continue;
        };
        if let Some(parsed) = parse_trace(&body) {
            return Some(parsed);
        }
    }
    None
}

fn parse_trace(body: &str) -> Option<(String, Option<String>)> {
    let get = |name: &str| {
        body.lines().find_map(|line| {
            line.split_once('=')
                .filter(|(k, _)| *k == name)
                .map(|(_, v)| v.trim())
        })
    };
    if !matches!(get("warp"), Some("on" | "plus")) {
        return None;
    }
    let ip = get("ip")?.parse::<std::net::IpAddr>().ok()?.to_string();
    let country = get("loc")
        .filter(|v| v.len() == 2 && v.bytes().all(|b| b.is_ascii_uppercase()))
        .map(str::to_owned);
    Some((ip, country))
}

fn sidecar_command(binary: &Path, log: &File) -> Result<Command> {
    let mut command = Command::new(binary);
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?);
    // Inherit neither an unrelated HTTP proxy nor any interactive console.
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(key);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }
    Ok(command)
}

struct OwnedChild {
    child: Child,
    #[cfg(windows)]
    _job: WindowsJob,
}

impl OwnedChild {
    fn spawn(command: &mut Command) -> Result<Self> {
        let mut child = command.spawn().context("无法启动内置 WARP 内核")?;
        #[cfg(windows)]
        {
            let job = match WindowsJob::attach(&child) {
                Ok(job) => job,
                Err(err) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(err);
                }
            };
            Ok(Self { child, _job: job })
        }
        #[cfg(not(windows))]
        {
            Ok(Self { child })
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(windows)]
struct WindowsJob {
    _handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
impl WindowsJob {
    fn attach(child: &Child) -> Result<Self> {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::JobObjects::*;
        // The OS kills only this app's sidecars if the app crashes or is terminated.
        unsafe {
            let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if handle.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            let owned = OwnedHandle::from_raw_handle(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                std::mem::size_of_val(&info) as u32,
            ) == 0
                || AssignProcessToJobObject(handle, child.as_raw_handle()) == 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("无法管理 WARP 子进程生命周期");
            }
            Ok(Self { _handle: owned })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_verified_warp_trace() {
        assert_eq!(
            parse_trace("ip=1.2.3.4\nwarp=on\nloc=JP\n"),
            Some(("1.2.3.4".into(), Some("JP".into())))
        );
        assert!(parse_trace("ip=1.2.3.4\nwarp=off\n").is_none());
        assert!(parse_trace("warp=on\nip=not-an-ip\n").is_none());
        assert!(parse_trace("<html>warp=on</html>").is_none());
        assert!(parse_trace("ip=::1\r\nwarp=plus\r\n").is_some());
    }

    #[tokio::test]
    async fn unavailable_warp_never_returns_a_direct_route() {
        let runtime = WarpRuntime::default();
        assert!(runtime.connect(false, false).await.is_err());
        assert!(!runtime.status().available);
        assert_eq!(runtime.status().phase, "error");
        assert!(runtime.proxy_url().is_err());
        assert_eq!(runtime.stop().await.phase, "stopped");
    }

    #[tokio::test]
    async fn registration_requires_explicit_terms_acceptance() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = WarpRuntime::new(Some(WarpPaths {
            binary: std::env::current_exe().unwrap(),
            data_dir: dir.path().join("warp"),
        }));
        let err = runtime.connect(false, false).await.unwrap_err();
        assert!(err.to_string().contains("服务条款"));
        assert!(!dir.path().join("warp").exists());
    }

    #[test]
    fn rejects_partial_identity_without_exposing_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        fs::write(&path, r#"{"private_key":"secret-value"}"#).unwrap();
        let err = validate_config(&path).unwrap_err().to_string();
        assert!(!err.contains("secret-value"));
        assert!(err.contains("endpoint_v4"));
    }

    #[test]
    fn child_fixture() {
        if std::env::var_os("STATEKIT_WARP_CHILD_FIXTURE").is_some() {
            std::thread::sleep(Duration::from_secs(60));
        }
    }

    #[test]
    fn owned_child_is_reaped_on_stop() {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "warp::tests::child_fixture"])
            .env("STATEKIT_WARP_CHILD_FIXTURE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = OwnedChild::spawn(&mut command).unwrap();
        assert!(child.child.try_wait().unwrap().is_none());
        let now = Instant::now();
        drop(child);
        assert!(now.elapsed() < Duration::from_secs(5));
    }
}
