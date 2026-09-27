//! Opens ChatGPT authorization in a normal Chrome window that Kit owns.
//!
//! The window uses a separate profile and the account's outbound proxy.
//! Google sign-in rejects the Codex CLI user agent, so the page gets a normal
//! Chrome user agent whose operating system follows the virtual device.
//! Host client hints stay disabled, or they would still say this computer is a Mac.

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::identity::{DevicePlatform, VmIdentity};

/// Destinations Chrome opens directly. `<-loopback>` would send localhost
/// through the proxy, so Kit would never see the authorization callback.
const LOCAL_BYPASS: &str = "localhost,127.0.0.1,::1,[::1]";

struct RunningBrowser {
    child: Child,
    profile: PathBuf,
    local_proxy: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for RunningBrowser {
    fn drop(&mut self) {
        stop_child(&mut self.child);
        if let Some(task) = self.local_proxy.take() {
            task.abort();
        }
        let _ = fs::remove_dir_all(&self.profile);
    }
}

static BROWSER: Mutex<Option<RunningBrowser>> = Mutex::const_new(None);

/// Opens `url` in Kit's authorization browser.
pub async fn open(url: &str, proxy: &str, identity: &VmIdentity) -> Result<()> {
    anyhow::ensure!(
        url.starts_with("https://auth.openai.com/"),
        "只能打开 ChatGPT 登录页"
    );
    let proxy_key = proxy.trim().to_string();
    let mut slot = BROWSER.lock().await;
    // Always start a fresh window. A Chrome already open on this profile
    // ignores new proxy flags and keeps the broken proxy from the last launch.
    *slot = None;
    stop_auth_browsers();
    let _ = fs::remove_dir_all(browser_root());
    *slot = Some(launch(url, &proxy_key, identity).await?);
    Ok(())
}

async fn launch(url: &str, proxy: &str, identity: &VmIdentity) -> Result<RunningBrowser> {
    let browser = find_browser().ok_or_else(|| {
        anyhow!("未找到 Chrome、Edge 或 Chromium。安装其中之一后才能打开授权页")
    })?;
    let profile = fresh_profile().context("无法创建授权浏览器配置目录")?;
    let local_proxy = if proxy.is_empty() {
        None
    } else {
        // Same relay as the rest of Kit, including the system-proxy chain.
        Some(spawn_local_proxy(&crate::outbound::dial_proxy_for_client(proxy)).await?)
    };
    let proxy_port = local_proxy.as_ref().map(|proxy| proxy.port);
    let user_agent = login_browser_user_agent(identity, &chrome_product_version(&browser));
    let child = match chrome_command(&browser, &profile, proxy_port, &user_agent, url).spawn() {
        Ok(child) => child,
        Err(err) => {
            let _ = fs::remove_dir_all(&profile);
            return Err(err).context("无法启动授权浏览器");
        }
    };
    Ok(RunningBrowser {
        child,
        profile,
        local_proxy: local_proxy.map(|proxy| proxy.task),
    })
}

fn chrome_command(
    browser: &std::path::Path,
    profile: &std::path::Path,
    proxy_port: Option<u16>,
    user_agent: &str,
    url: &str,
) -> Command {
    let mut command = Command::new(browser);
    command
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-sync")
        .arg("--disable-features=UserAgentClientHint")
        .arg(format!("--user-agent={user_agent}"));
    if let Some(port) = proxy_port {
        // Switches must come before the URL. Chrome stops reading flags at
        // the first page address, which previously left it on the system proxy.
        command.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
        // The login result is delivered to Kit at http://localhost:1455 or
        // :1457. Those addresses must reach this computer, not the proxy.
        command.arg(format!("--proxy-bypass-list={LOCAL_BYPASS}"));
    } else {
        command.arg("--no-proxy-server");
    }
    command
        .arg("--new-window")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

struct LocalProxy {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

async fn spawn_local_proxy(upstream: &str) -> Result<LocalProxy> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("无法打开授权浏览器的本地代理")?;
    let port = listener.local_addr()?.port();
    let upstream = upstream.to_string();
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let upstream = upstream.clone();
            tokio::spawn(async move {
                if let Err(err) = serve_proxy_client(stream, &upstream).await {
                    eprintln!("[auth-browser] 代理转发失败: {err:#}");
                }
            });
        }
    });
    Ok(LocalProxy { port, task })
}

async fn serve_proxy_client(mut inbound: tokio::net::TcpStream, upstream: &str) -> Result<()> {
    let mut header = Vec::new();
    let mut byte = [0u8; 1];
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() > 16 * 1024 {
            anyhow::bail!("代理请求过长");
        }
        if inbound.read(&mut byte).await? == 0 {
            return Ok(());
        }
        header.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&header);
    let request = text.lines().next().unwrap_or("");
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if !method.eq_ignore_ascii_case("CONNECT") {
        inbound
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
            .await
            .ok();
        return Ok(());
    }
    let (host, port) = split_host_port(target).context("CONNECT 目标无效")?;
    match crate::ws_upstream::dial_tcp(upstream, &host, port).await {
        Ok(mut outbound) => {
            inbound
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await?;
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
            Ok(())
        }
        Err(err) => {
            let body = err.to_string();
            let response = format!(
                "HTTP/1.1 502 Bad Gateway\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            inbound.write_all(response.as_bytes()).await.ok();
            Err(err)
        }
    }
}

fn split_host_port(target: &str) -> Option<(String, u16)> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, port) = rest.split_once("]:")?;
        return Some((host.to_string(), port.parse().ok()?));
    }
    let (host, port) = target.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

fn login_browser_user_agent(identity: &VmIdentity, chrome_version: &str) -> String {
    let os = match identity.platform() {
        DevicePlatform::Mac => "(Macintosh; Intel Mac OS X 10_15_7)",
        DevicePlatform::Windows => match identity.arch.as_str() {
            "arm64" | "aarch64" => "(Windows NT 10.0; ARM64)",
            _ => "(Windows NT 10.0; Win64; x64)",
        },
        DevicePlatform::Linux => match identity.arch.as_str() {
            "arm64" | "aarch64" => "(X11; Linux aarch64)",
            _ => "(X11; Linux x86_64)",
        },
    };
    format!(
        "Mozilla/5.0 {os} AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{chrome_version} Safari/537.36"
    )
}

fn chrome_product_version(browser: &std::path::Path) -> String {
    Command::new(browser)
        .arg("--version")
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .as_deref()
        .and_then(parse_chrome_version)
        .unwrap_or_else(|| "131.0.0.0".into())
}

fn parse_chrome_version(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|part| {
        let mut pieces = part.split('.');
        let major = pieces.next()?;
        if !major.chars().all(|ch| ch.is_ascii_digit()) || pieces.next().is_none() {
            return None;
        }
        Some(part.trim_end_matches(|ch: char| !ch.is_ascii_digit() && ch != '.').to_string())
    })
}

fn browser_root() -> PathBuf {
    let name = if crate::settings::is_dev_mode() {
        ".codex-state-kit-dev-auth-browser"
    } else {
        ".codex-state-kit-auth-browser"
    };
    crate::settings::home_dir().join(name)
}

fn fresh_profile() -> Result<PathBuf> {
    let root = browser_root();
    fs::create_dir_all(&root)?;
    let profile = root.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir(&profile)?;
    Ok(profile)
}

fn find_browser() -> Option<PathBuf> {
    let candidates = [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/microsoft-edge",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ];
    candidates
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .or_else(|| {
            ["google-chrome", "chromium", "chromium-browser", "microsoft-edge"]
                .into_iter()
                .find_map(|name| which(name))
        })
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        candidate.is_file().then_some(candidate)
    })
}

fn stop_auth_browsers() {
    let root = browser_root();
    let Some(path) = root.to_str() else {
        return;
    };
    // The brackets keep pkill from matching its own command line.
    let pattern = format!("user-data-di[r]={path}");
    let _ = Command::new("pkill")
        .args(["-f", &pattern])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    std::thread::sleep(std::time::Duration::from_millis(200));
}

fn stop_child(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        libc_kill(-(child.id() as i32), 15);
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_login_page_uses_the_virtual_device_user_agent() {
        let mut identity = VmIdentity::ephemeral();
        identity.apply_profile(crate::identity::VmProfile {
            platform: DevicePlatform::Windows,
            environment: None,
            enabled: Some(true),
            terminal: None,
            terminal_version: None,
            terminal_multiplexer: None,
        });
        let agent = login_browser_user_agent(&identity, "142.0.7444.60");
        assert!(agent.starts_with("Mozilla/5.0 (Windows NT 10.0; Win64; x64)"));
        assert!(agent.contains("Chrome/142.0.7444.60"));
        assert!(!agent.contains("Macintosh"));
        assert!(!agent.contains("codex_cli_rs"));
        assert_eq!(
            parse_chrome_version("Google Chrome 142.0.7444.60 \n").as_deref(),
            Some("142.0.7444.60")
        );
    }

    #[test]
    fn each_profile_name_is_unique() {
        let root = std::path::Path::new("/tmp/kit-auth-browser");
        let first = root.join(uuid::Uuid::new_v4().to_string());
        let second = root.join(uuid::Uuid::new_v4().to_string());
        assert_ne!(first, second);
        assert!(first.starts_with(root));
    }

    #[test]
    fn login_callback_stays_on_this_computer() {
        assert!(LOCAL_BYPASS.contains("localhost"));
        assert!(LOCAL_BYPASS.contains("127.0.0.1"));
        assert!(!LOCAL_BYPASS.contains("<-loopback>"));
    }

    #[test]
    fn connect_target_splits_host_and_port() {
        assert_eq!(
            split_host_port("auth.openai.com:443").unwrap(),
            ("auth.openai.com".into(), 443)
        );
        assert_eq!(split_host_port("[::1]:443").unwrap(), ("::1".into(), 443));
    }
}
