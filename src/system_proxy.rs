//! Reaching the manual outbound proxy through the OS system proxy.
//!
//! Clash Verge and similar tools often run with only the system proxy on
//! (no TUN). Browsers then go through Clash, but Kit's own TCP connection to a
//! manual SOCKS5/HTTP proxy abroad does not, and fails wherever that server
//! is only reachable through Clash.
//!
//! Every manual proxy that is not on this machine is therefore reached through
//! a small local relay (`127.0.0.1:<port>` → proxy server). For each new
//! connection the relay checks the setting and the current system proxy: when
//! enabled and one is set, it dials the proxy server through it (HTTP CONNECT
//! or SOCKS5), otherwise directly. Turning Clash's system proxy on or off
//! therefore applies to the next connection without restarting Kit.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const DETECT_TTL: Duration = Duration::from_secs(5);
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

static ENABLED: AtomicBool = AtomicBool::new(true);
static DETECTED: Mutex<Option<(Instant, Option<FrontProxy>)>> = Mutex::new(None);
static RELAYS: OnceLock<Mutex<HashMap<(String, u16), u16>>> = OnceLock::new();
static RUNTIME: OnceLock<Option<tokio::runtime::Runtime>> = OnceLock::new();
static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);
#[cfg(test)]
static OVERRIDE: Mutex<Option<Option<FrontProxy>>> = Mutex::new(None);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrontKind {
    Http,
    Socks5,
}

/// The system proxy Kit dials through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrontProxy {
    pub kind: FrontKind,
    pub host: String,
    pub port: u16,
}

impl FrontProxy {
    pub fn label(&self) -> String {
        let kind = match self.kind {
            FrontKind::Http => "HTTP",
            FrontKind::Socks5 => "SOCKS5",
        };
        format!("{kind} {}:{}", self.host, self.port)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemProxyView {
    pub enabled: bool,
    /// The system proxy currently detected, e.g. "HTTP 127.0.0.1:7897".
    pub detected: Option<String>,
    /// Last failure to reach the manual proxy through the relay.
    pub last_error: Option<String>,
}

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn view() -> SystemProxyView {
    SystemProxyView {
        enabled: enabled(),
        detected: detect().map(|front| front.label()),
        last_error: LAST_ERROR.lock().ok().and_then(|slot| slot.clone()),
    }
}

/// The system proxy, cached for a few seconds (detection runs a process on
/// Windows and macOS).
pub fn detect() -> Option<FrontProxy> {
    #[cfg(test)]
    if let Some(forced) = OVERRIDE.lock().unwrap().clone() {
        return forced;
    }
    let mut slot = DETECTED.lock().unwrap_or_else(|error| error.into_inner());
    if let Some((at, value)) = slot.as_ref() {
        if at.elapsed() < DETECT_TTL {
            return value.clone();
        }
    }
    let value = detect_uncached();
    *slot = Some((Instant::now(), value.clone()));
    value
}

fn detect_uncached() -> Option<FrontProxy> {
    platform_proxy().or_else(env_proxy)
}

fn parse_front(raw: &str, default_kind: FrontKind) -> Option<FrontProxy> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let with_scheme = if raw.contains("://") {
        raw.to_string()
    } else {
        let scheme = match default_kind {
            FrontKind::Http => "http",
            FrontKind::Socks5 => "socks5",
        };
        format!("{scheme}://{raw}")
    };
    let url = url::Url::parse(&with_scheme).ok()?;
    let kind = match url.scheme() {
        "http" => FrontKind::Http,
        "socks5" | "socks5h" | "socks" => FrontKind::Socks5,
        _ => return None,
    };
    let host = url
        .host_str()?
        .trim_matches(|c| c == '[' || c == ']')
        .to_string();
    let port = url.port().unwrap_or(match kind {
        FrontKind::Http => 80,
        FrontKind::Socks5 => 1080,
    });
    Some(FrontProxy { kind, host, port })
}

fn env_proxy() -> Option<FrontProxy> {
    [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ]
    .iter()
    .find_map(|name| {
        std::env::var(name)
            .ok()
            .and_then(|value| parse_front(&value, FrontKind::Http))
    })
}

#[cfg(windows)]
fn platform_proxy() -> Option<FrontProxy> {
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
        ])
        .creation_flags(0x08000000)
        .output()
        .ok()?;
    parse_windows_registry(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `reg query` output for ProxyEnable / ProxyServer.
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_windows_registry(text: &str) -> Option<FrontProxy> {
    let value = |name: &str| {
        text.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next()? == name).then(|| parts.nth(1).map(str::to_string))?
        })
    };
    let enabled = value("ProxyEnable")
        .and_then(|raw| u32::from_str_radix(raw.trim_start_matches("0x"), 16).ok())
        .unwrap_or(0);
    if enabled == 0 {
        return None;
    }
    let server = value("ProxyServer")?;
    if !server.contains('=') {
        return parse_front(&server, FrontKind::Http);
    }
    // "http=host:port;https=host:port;socks=host:port"
    let entry = |key: &str| {
        server.split(';').find_map(|item| {
            let (name, address) = item.split_once('=')?;
            name.trim()
                .eq_ignore_ascii_case(key)
                .then(|| address.trim().to_string())
        })
    };
    entry("https")
        .and_then(|address| parse_front(&address, FrontKind::Http))
        .or_else(|| entry("http").and_then(|address| parse_front(&address, FrontKind::Http)))
        .or_else(|| entry("socks").and_then(|address| parse_front(&address, FrontKind::Socks5)))
}

#[cfg(target_os = "macos")]
fn platform_proxy() -> Option<FrontProxy> {
    let output = std::process::Command::new("scutil")
        .arg("--proxy")
        .output()
        .ok()?;
    parse_scutil(&String::from_utf8_lossy(&output.stdout))
}

/// Parses `scutil --proxy` (HTTPS, then HTTP, then SOCKS).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_scutil(text: &str) -> Option<FrontProxy> {
    let value = |key: &str| {
        text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim() == key).then(|| value.trim().to_string())
        })
    };
    [
        ("HTTPS", FrontKind::Http),
        ("HTTP", FrontKind::Http),
        ("SOCKS", FrontKind::Socks5),
    ]
    .into_iter()
    .find_map(|(prefix, kind)| {
        if value(&format!("{prefix}Enable")).as_deref() != Some("1") {
            return None;
        }
        let host = value(&format!("{prefix}Proxy"))?;
        let port = value(&format!("{prefix}Port"))?.parse().ok()?;
        Some(FrontProxy { kind, host, port })
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn platform_proxy() -> Option<FrontProxy> {
    let get = |schema: &str, key: &str| {
        let output = std::process::Command::new("gsettings")
            .args(["get", schema, key])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_matches('\'')
            .to_string();
        (!text.is_empty()).then_some(text)
    };
    if get("org.gnome.system.proxy", "mode")? != "manual" {
        return None;
    }
    [
        ("org.gnome.system.proxy.https", FrontKind::Http),
        ("org.gnome.system.proxy.http", FrontKind::Http),
        ("org.gnome.system.proxy.socks", FrontKind::Socks5),
    ]
    .into_iter()
    .find_map(|(schema, kind)| {
        let host = get(schema, "host")?;
        let port: u16 = get(schema, "port")?.parse().ok().filter(|port| *port > 0)?;
        Some(FrontProxy { kind, host, port })
    })
}

#[cfg(not(any(unix, windows)))]
fn platform_proxy() -> Option<FrontProxy> {
    None
}

/// For traffic that does not use the outbound line (ChatGPT login, token
/// import): follow the system proxy like a browser does, when enabled.
pub fn reqwest_proxy() -> reqwest::Proxy {
    reqwest::Proxy::custom(|url| {
        if !enabled() || url.host_str().is_some_and(is_local) {
            return None;
        }
        detect().map(|front| match front.kind {
            FrontKind::Http => format!("http://{}:{}", front.host, front.port),
            FrontKind::Socks5 => format!("socks5h://{}:{}", front.host, front.port),
        })
    })
}

fn is_local(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(|c| c == '[' || c == ']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified())
}

/// A small dedicated runtime so relays work no matter which thread (or
/// runtime) first asks for one, including startup code outside Tokio.
fn runtime() -> Option<&'static tokio::runtime::Runtime> {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("kit-proxy-relay")
                .enable_all()
                .build()
                .map_err(|error| eprintln!("[system-proxy] 无法启动中转: {error}"))
                .ok()
        })
        .as_ref()
}

/// Rewrites a client proxy URL (`socks5h://user:pass@host:port`) to go
/// through a local relay for `host:port`. Local proxies (Kit's embedded
/// subscription core, Clash itself) and TLS (`https://`) proxies are
/// returned unchanged.
pub fn route(client_url: &str) -> String {
    let Ok(mut url) = url::Url::parse(client_url.trim()) else {
        return client_url.to_string();
    };
    if !matches!(
        url.scheme(),
        "http" | "socks5" | "socks5h" | "socks4" | "socks4a"
    ) {
        return client_url.to_string();
    }
    let Some(host) = url.host_str().map(str::to_string) else {
        return client_url.to_string();
    };
    if is_local(&host) {
        return client_url.to_string();
    }
    let port = url.port_or_known_default().unwrap_or(match url.scheme() {
        "http" => 80,
        _ => 1080,
    });
    let Some(local) = relay_port(&host, port) else {
        return client_url.to_string();
    };
    if url.set_host(Some("127.0.0.1")).is_err() || url.set_port(Some(local)).is_err() {
        return client_url.to_string();
    }
    url.to_string()
}

fn relay_port(host: &str, port: u16) -> Option<u16> {
    let relays = RELAYS.get_or_init(Default::default);
    let mut relays = relays.lock().unwrap_or_else(|error| error.into_inner());
    let key = (host.to_ascii_lowercase(), port);
    if let Some(local) = relays.get(&key) {
        return Some(*local);
    }
    let runtime = runtime()?;
    let listener = std::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).ok()?;
    listener.set_nonblocking(true).ok()?;
    let local = listener.local_addr().ok()?.port();
    let (target_host, target_port) = (host.to_string(), port);
    runtime.spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("[system-proxy] 中转监听失败: {error}");
                return;
            }
        };
        loop {
            let Ok((inbound, _)) = listener.accept().await else {
                continue;
            };
            let host = target_host.clone();
            tokio::spawn(relay(inbound, host, target_port));
        }
    });
    relays.insert(key, local);
    Some(local)
}

async fn relay(mut inbound: TcpStream, host: String, port: u16) {
    let front = if enabled() {
        tokio::task::spawn_blocking(detect).await.ok().flatten()
    } else {
        None
    };
    let dialed = match tokio::time::timeout(DIAL_TIMEOUT, dial(front.as_ref(), &host, port)).await {
        Ok(result) => result.map_err(|error| format!("{error:#}")),
        Err(_) => Err("连接超时".to_string()),
    };
    let mut outbound = match dialed {
        Ok(stream) => {
            if let Ok(mut slot) = LAST_ERROR.lock() {
                *slot = None;
            }
            stream
        }
        Err(reason) => {
            let message = match &front {
                Some(front) => format!(
                    "经系统代理 {} 连接 {host}:{port} 失败：{reason}",
                    front.label()
                ),
                None => format!("直连代理服务器 {host}:{port} 失败：{reason}"),
            };
            eprintln!("[system-proxy] {message}");
            if let Ok(mut slot) = LAST_ERROR.lock() {
                *slot = Some(message);
            }
            return;
        }
    };
    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
}

/// Opens a TCP stream to `host:port`, through `front` when given.
pub async fn dial(front: Option<&FrontProxy>, host: &str, port: u16) -> Result<TcpStream> {
    let Some(front) = front else {
        return TcpStream::connect((host, port))
            .await
            .with_context(|| format!("无法连接 {host}:{port}"));
    };
    let mut stream = TcpStream::connect((front.host.as_str(), front.port))
        .await
        .with_context(|| format!("无法连接系统代理 {}", front.label()))?;
    stream.set_nodelay(true).ok();
    match front.kind {
        FrontKind::Http => http_connect(&mut stream, host, port).await?,
        FrontKind::Socks5 => socks5_connect(&mut stream, host, port).await?,
    }
    Ok(stream)
}

async fn http_connect(stream: &mut TcpStream, host: &str, port: u16) -> Result<()> {
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    stream
        .write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await?;
    let mut response = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        if response.len() > 8192 {
            bail!("系统代理响应过长");
        }
        if stream.read(&mut byte).await? == 0 {
            bail!("系统代理提前关闭了连接");
        }
        response.push(byte[0]);
    }
    let status = String::from_utf8_lossy(&response);
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        bail!(
            "系统代理拒绝了 CONNECT（{}）",
            status.lines().next().unwrap_or("").trim()
        );
    }
    Ok(())
}

async fn socks5_connect(stream: &mut TcpStream, host: &str, port: u16) -> Result<()> {
    stream.write_all(&[5, 1, 0]).await?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await?;
    if reply != [5, 0] {
        bail!("系统代理不支持无认证 SOCKS5");
    }
    let mut request = vec![5, 1, 0];
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            request.push(1);
            request.extend_from_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            request.push(4);
            request.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            let name = host.as_bytes();
            if name.len() > 255 {
                bail!("主机名过长");
            }
            request.push(3);
            request.push(name.len() as u8);
            request.extend_from_slice(name);
        }
    }
    request.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&request).await?;
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[1] != 0 {
        bail!("系统代理 SOCKS5 连接失败（代码 {}）", head[1]);
    }
    let skip = match head[3] {
        1 => 4,
        4 => 16,
        3 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            len[0] as usize
        }
        _ => bail!("系统代理 SOCKS5 响应无效"),
    };
    let mut rest = vec![0u8; skip + 2];
    stream.read_exact(&mut rest).await?;
    Ok(())
}

#[cfg(test)]
pub(crate) fn override_detected(value: Option<Option<FrontProxy>>) {
    *OVERRIDE.lock().unwrap() = value;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn parses_windows_and_macos_settings() {
        let reg = "HKEY_CURRENT_USER\\...\\Internet Settings\r\n    ProxyEnable    REG_DWORD    0x1\r\n    ProxyServer    REG_SZ    127.0.0.1:7897\r\n";
        assert_eq!(
            parse_windows_registry(reg),
            Some(FrontProxy {
                kind: FrontKind::Http,
                host: "127.0.0.1".into(),
                port: 7897
            })
        );
        let split = "    ProxyEnable    REG_DWORD    0x1\n    ProxyServer    REG_SZ    socks=127.0.0.1:7891;https=127.0.0.1:7890\n";
        assert_eq!(parse_windows_registry(split).unwrap().port, 7890);
        let off =
            "    ProxyEnable    REG_DWORD    0x0\n    ProxyServer    REG_SZ    127.0.0.1:7897\n";
        assert_eq!(parse_windows_registry(off), None);

        let scutil = "<dictionary> {\n  HTTPEnable : 0\n  HTTPSEnable : 1\n  HTTPSPort : 7897\n  HTTPSProxy : 127.0.0.1\n  SOCKSEnable : 1\n  SOCKSPort : 7898\n  SOCKSProxy : 127.0.0.1\n}\n";
        assert_eq!(
            parse_scutil(scutil),
            Some(FrontProxy {
                kind: FrontKind::Http,
                host: "127.0.0.1".into(),
                port: 7897
            })
        );
        assert_eq!(parse_scutil("<dictionary> {\n  HTTPSEnable : 0\n}\n"), None);
    }

    #[test]
    fn local_and_tls_proxies_are_not_relayed() {
        assert_eq!(
            route("socks5h://127.0.0.1:7897"),
            "socks5h://127.0.0.1:7897"
        );
        assert_eq!(route("http://localhost:8080"), "http://localhost:8080");
        assert_eq!(
            route("https://proxy.example.com:443"),
            "https://proxy.example.com:443"
        );
        let routed = route("socks5h://user:pass@proxy.example.com:3010");
        assert!(
            routed.starts_with("socks5h://user:pass@127.0.0.1:"),
            "{routed}"
        );
        // Same target, same relay.
        assert_eq!(routed, route("socks5h://user:pass@proxy.example.com:3010"));
    }

    /// A fake Clash (HTTP CONNECT) and a fake remote server behind it.
    #[tokio::test]
    async fn relays_through_the_system_proxy_when_enabled() {
        let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_port = remote.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = remote.accept().await.unwrap();
            socket.write_all(b"hello-from-remote").await.unwrap();
        });
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_port = front.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(Mutex::new(String::new()));
        let seen_by_front = seen.clone();
        tokio::spawn(async move {
            let (mut client, _) = front.accept().await.unwrap();
            let mut buffer = vec![0u8; 512];
            let read = client.read(&mut buffer).await.unwrap();
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            *seen_by_front.lock().unwrap() = request.clone();
            let target = request.split_whitespace().nth(1).unwrap().to_string();
            let mut upstream = TcpStream::connect(target).await.unwrap();
            client
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        });
        let front = FrontProxy {
            kind: FrontKind::Http,
            host: "127.0.0.1".into(),
            port: front_port,
        };
        let mut stream = dial(Some(&front), "127.0.0.1", remote_port).await.unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).await.unwrap();
        assert_eq!(body, "hello-from-remote");
        assert!(seen
            .lock()
            .unwrap()
            .starts_with(&format!("CONNECT 127.0.0.1:{remote_port} HTTP/1.1")));
    }

    #[tokio::test]
    async fn socks5_front_is_supported() {
        let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let remote_port = remote.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = remote.accept().await.unwrap();
            socket.write_all(b"ok").await.unwrap();
        });
        let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let front_port = front.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut client, _) = front.accept().await.unwrap();
            let mut greeting = [0u8; 3];
            client.read_exact(&mut greeting).await.unwrap();
            client.write_all(&[5, 0]).await.unwrap();
            let mut head = [0u8; 4];
            client.read_exact(&mut head).await.unwrap();
            assert_eq!(head[3], 1);
            let mut addr = [0u8; 6];
            client.read_exact(&mut addr).await.unwrap();
            let port = u16::from_be_bytes([addr[4], addr[5]]);
            let mut upstream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            client
                .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        });
        let front = FrontProxy {
            kind: FrontKind::Socks5,
            host: "127.0.0.1".into(),
            port: front_port,
        };
        let mut stream = dial(Some(&front), "127.0.0.1", remote_port).await.unwrap();
        let mut body = String::new();
        stream.read_to_string(&mut body).await.unwrap();
        assert_eq!(body, "ok");
    }

    /// Accepts SOCKS5 CONNECT requests and forwards them (the "proxy abroad").
    async fn fake_socks5_server() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut greeting = [0u8; 2];
                    client.read_exact(&mut greeting).await.unwrap();
                    let mut methods = vec![0u8; greeting[1] as usize];
                    client.read_exact(&mut methods).await.unwrap();
                    client.write_all(&[5, 0]).await.unwrap();
                    let mut head = [0u8; 4];
                    client.read_exact(&mut head).await.unwrap();
                    let host = match head[3] {
                        1 => {
                            let mut ip = [0u8; 4];
                            client.read_exact(&mut ip).await.unwrap();
                            Ipv4Addr::from(ip).to_string()
                        }
                        3 => {
                            let mut len = [0u8; 1];
                            client.read_exact(&mut len).await.unwrap();
                            let mut name = vec![0u8; len[0] as usize];
                            client.read_exact(&mut name).await.unwrap();
                            String::from_utf8(name).unwrap()
                        }
                        other => panic!("unexpected atyp {other}"),
                    };
                    let mut port = [0u8; 2];
                    client.read_exact(&mut port).await.unwrap();
                    let mut upstream =
                        TcpStream::connect((host.as_str(), u16::from_be_bytes(port)))
                            .await
                            .unwrap();
                    client
                        .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        port
    }

    /// Plays Clash: an HTTP CONNECT proxy that records where it connected.
    async fn fake_clash() -> (u16, std::sync::Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let (mut client, _) = listener.accept().await.unwrap();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut byte = [0u8; 1];
                    while !request.ends_with(b"\r\n\r\n") {
                        client.read_exact(&mut byte).await.unwrap();
                        request.push(byte[0]);
                    }
                    let target = String::from_utf8_lossy(&request)
                        .split_whitespace()
                        .nth(1)
                        .unwrap()
                        .to_string();
                    log.lock().unwrap().push(target.clone());
                    let mut upstream = TcpStream::connect(target).await.unwrap();
                    client
                        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
                });
            }
        });
        (port, seen)
    }

    /// reqwest → relay → system proxy → manual SOCKS5 proxy → target.
    #[tokio::test]
    async fn business_clients_reach_the_manual_proxy_through_the_system_proxy() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = target.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut buffer = [0u8; 1024];
                    let _ = socket.read(&mut buffer).await;
                    socket
                        .write_all(
                            b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                        )
                        .await
                        .unwrap();
                });
            }
        });
        let socks_port = fake_socks5_server().await;
        let (clash_port, seen) = fake_clash().await;
        override_detected(Some(Some(FrontProxy {
            kind: FrontKind::Http,
            host: "127.0.0.1".into(),
            port: clash_port,
        })));
        // The socks server is local in this test, so relay it explicitly.
        let relay = relay_port("127.0.0.1", socks_port).unwrap();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("socks5h://127.0.0.1:{relay}")).unwrap())
            .build()
            .unwrap();
        let body = client
            .get(format!("http://127.0.0.1:{target_port}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        override_detected(None);
        assert_eq!(body, "ok");
        assert_eq!(
            seen.lock().unwrap().as_slice(),
            [format!("127.0.0.1:{socks_port}")]
        );
    }

    #[test]
    fn a_refused_connect_is_an_error() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let front_port = front.local_addr().unwrap().port();
            tokio::spawn(async move {
                let (mut client, _) = front.accept().await.unwrap();
                let mut buffer = [0u8; 256];
                let _ = client.read(&mut buffer).await;
                client
                    .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
                    .await
                    .unwrap();
            });
            let front = FrontProxy {
                kind: FrontKind::Http,
                host: "127.0.0.1".into(),
                port: front_port,
            };
            let error = dial(Some(&front), "example.com", 443).await.unwrap_err();
            assert!(format!("{error:#}").contains("403"));
        });
    }
}
