//! 到 Codex 上游的单条 WebSocket 连接。
//!
//! 一条连接一次只能跑一轮。`open_turn` 持有连接锁直到收到结束事件，HTTP 桥和客户端
//! WebSocket 共用它。连接超过 55 分钟、认证或代理变化时会在下一轮重建。
//!
//! tungstenite 0.26 的 `WebSocketConfig` 没有 permessage-deflate，这里用默认配置。

use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, OwnedMutexGuard};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tokio_tungstenite::WebSocketStream;
use url::Url;

use crate::fetch;
use crate::ws_bridge;

const MAX_AGE: Duration = Duration::from_secs(55 * 60);
const OPENAI_BETA: &str = "responses_websockets=2026-02-06";

#[derive(Clone, Debug)]
pub struct WsSnapshot {
    pub connected: bool,
    pub connected_at: Option<String>,
}

#[derive(Clone)]
pub struct WsDial {
    pub url: String,
    pub proxy: String,
    pub authorization: String,
    pub account_id: String,
    pub extra_headers: Vec<(String, String)>,
}

impl WsDial {
    fn key(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}",
            self.url, self.proxy, self.authorization, self.account_id
        )
    }
}

#[derive(Clone, Default)]
pub struct WsUpstreamPool {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    live: Option<Live>,
    connected_at: Option<String>,
    sticky_session: Option<String>,
}

struct Live {
    ws: WebSocketStream<BoxIo>,
    connected_at: Instant,
    key: String,
}

enum Read {
    Event(String),
    Fail(String),
}

impl WsUpstreamPool {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn snapshot(&self) -> WsSnapshot {
        let guard = self.inner.lock().await;
        let connected = guard.live.as_ref().is_some_and(|live| {
            live.connected_at.elapsed() < MAX_AGE
        });
        WsSnapshot {
            connected,
            connected_at: connected.then(|| guard.connected_at.clone()).flatten(),
        }
    }

    pub async fn invalidate(&self) {
        let mut guard = self.inner.lock().await;
        guard.live = None;
        guard.connected_at = None;
        guard.sticky_session = None;
    }

    /// `{session}` 在一条连接的存活期内保持不变。调用方已解析好的地址原样返回。
    pub async fn resolve_proxy(&self, template: &str, bound: Option<&str>) -> Result<String> {
        let template = template.trim();
        if !fetch::has_session_placeholder(template) {
            return Ok(fetch::outbound_proxy_for_client(template));
        }
        if let Some(session) = bound.map(str::trim).filter(|value| !value.is_empty()) {
            return Ok(fetch::outbound_proxy_for_client(
                &fetch::apply_bound_session(template, Some(session))?,
            ));
        }
        let mut guard = self.inner.lock().await;
        if guard.sticky_session.is_none() {
            guard.sticky_session = Some(fetch::generate_proxy_session());
        }
        let session = guard
            .sticky_session
            .clone()
            .context("缺少代理 session")?;
        Ok(fetch::outbound_proxy_for_client(
            &fetch::replace_session_placeholder(template, &session),
        ))
    }

    pub async fn open_turn(
        &self,
        dial: WsDial,
        payload: Value,
    ) -> Result<mpsc::Receiver<Result<String, String>>> {
        let mut guard = self.inner.clone().lock_owned().await;
        ensure(&mut guard, &dial).await?;
        if let Err(err) = send_frame(&mut guard, &payload).await {
            guard.live = None;
            guard.connected_at = None;
            return Err(err);
        }
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            drive(guard, dial, payload, tx).await;
        });
        Ok(rx)
    }
}

fn connection_fresh(age: Duration) -> bool {
    age < MAX_AGE
}

fn idle_timeout(saw_event: bool) -> Duration {
    #[cfg(test)]
    if let Ok(ms) = std::env::var("CSK_SSE_IDLE_MS") {
        if let Ok(ms) = ms.parse::<u64>() {
            return Duration::from_millis(ms.max(1));
        }
    }
    if saw_event {
        Duration::from_secs(90)
    } else {
        Duration::from_secs(180)
    }
}

async fn ensure(inner: &mut Inner, dial: &WsDial) -> Result<()> {
    let key = dial.key();
    let fresh = inner.live.as_ref().is_some_and(|live| {
        live.key == key && connection_fresh(live.connected_at.elapsed())
    });
    if fresh {
        return Ok(());
    }
        let replacing = inner.live.is_some();
        inner.live = None;
        inner.connected_at = None;
        if replacing {
            inner.sticky_session = None;
        }
        let ws = connect_upstream(dial).await?;
    inner.connected_at = Some(chrono::Utc::now().to_rfc3339());
    inner.live = Some(Live {
        ws,
        connected_at: Instant::now(),
        key,
    });
    Ok(())
}

async fn send_frame(inner: &mut Inner, payload: &Value) -> Result<()> {
    let live = inner
        .live
        .as_mut()
        .context("上游 WebSocket 未连接")?;
    let text = serde_json::to_string(payload).context("无法序列化 WebSocket 请求")?;
    live.ws
        .send(Message::text(text))
        .await
        .context("发送上游 WebSocket 帧失败")?;
    Ok(())
}

async fn drive(
    mut guard: OwnedMutexGuard<Inner>,
    dial: WsDial,
    mut payload: Value,
    tx: mpsc::Sender<Result<String, String>>,
) {
    let mut saw_event = false;
    let mut retried_limit = false;
    let mut retried_missing = false;
    loop {
        match read_one(&mut guard, saw_event).await {
            Read::Event(text) => {
                if !saw_event
                    && !retried_limit
                    && ws_bridge::ws_error_code(&text).as_deref()
                        == Some("websocket_connection_limit_reached")
                {
                    retried_limit = true;
                    guard.live = None;
                    guard.connected_at = None;
                    guard.sticky_session = None;
                    if ensure(&mut guard, &dial).await.is_err()
                        || send_frame(&mut guard, &payload).await.is_err()
                    {
                        let _ = tx
                            .send(Err("上游 WebSocket 达到连接时限，重连失败".into()))
                            .await;
                        break;
                    }
                    continue;
                }
                if !saw_event
                    && !retried_missing
                    && ws_bridge::ws_error_code(&text).as_deref()
                        == Some("previous_response_not_found")
                {
                    retried_missing = true;
                    ws_bridge::strip_previous_response_id(&mut payload);
                    if send_frame(&mut guard, &payload).await.is_err() {
                        guard.live = None;
                        guard.connected_at = None;
                        let _ = tx
                            .send(Err("链式响应已失效，重发失败".into()))
                            .await;
                        break;
                    }
                    continue;
                }
                let terminal = ws_bridge::is_terminal_event(&text);
                saw_event = true;
                if tx.send(Ok(text)).await.is_err() {
                    guard.live = None;
                    guard.connected_at = None;
                    break;
                }
                if terminal {
                    break;
                }
            }
            Read::Fail(message) => {
                guard.live = None;
                guard.connected_at = None;
                let _ = tx.send(Err(message)).await;
                break;
            }
        }
    }
}

async fn read_one(inner: &mut Inner, saw_event: bool) -> Read {
    let idle = idle_timeout(saw_event);
    loop {
        if inner.live.is_none() {
            return Read::Fail("上游 WebSocket 未连接".into());
        }
        let message = {
            let live = inner.live.as_mut().expect("live");
            if !connection_fresh(live.connected_at.elapsed()) {
                return Read::Fail("上游 WebSocket 连接已到期".into());
            }
            tokio::time::timeout(idle, live.ws.next()).await
        };
        match message {
            Err(_) => {
                return Read::Fail(if saw_event {
                    "上游 WebSocket 在事件之间静默超时".into()
                } else {
                    "上游 WebSocket 在首个事件前静默超时".into()
                });
            }
            Ok(None) => return Read::Fail("上游 WebSocket 连接已关闭".into()),
            Ok(Some(Err(err))) => {
                return Read::Fail(format!("上游 WebSocket 读取失败: {err}"));
            }
            Ok(Some(Ok(Message::Ping(payload)))) => {
                let live = inner.live.as_mut().expect("live");
                if live.ws.send(Message::Pong(payload)).await.is_err() {
                    return Read::Fail("上游 WebSocket Pong 失败".into());
                }
            }
            Ok(Some(Ok(Message::Close(_)))) => {
                return Read::Fail("上游 WebSocket 连接已关闭".into());
            }
            Ok(Some(Ok(Message::Text(text)))) => return Read::Event(text.to_string()),
            Ok(Some(Ok(Message::Pong(_) | Message::Binary(_) | Message::Frame(_)))) => {}
        }
    }
}

async fn connect_upstream(dial: &WsDial) -> Result<WebSocketStream<BoxIo>> {
    let url = Url::parse(&dial.url).context("上游 WebSocket 地址无效")?;
    let mut request = dial
        .url
        .clone()
        .into_client_request()
        .context("无法构造 WebSocket 握手")?;
    let headers = request.headers_mut();
    insert_header(headers, "OpenAI-Beta", OPENAI_BETA);
    if !dial.authorization.is_empty() {
        insert_header(headers, "Authorization", &dial.authorization);
    }
    if !dial.account_id.is_empty() {
        insert_header(headers, "ChatGPT-Account-ID", &dial.account_id);
    }
    for (name, value) in &dial.extra_headers {
        insert_header(headers, name, value);
    }
    let io = dial_io(&url, &dial.proxy).await?;
    let (stream, _response) = tokio_tungstenite::client_async_with_config(request, io, None)
        .await
        .context("上游 WebSocket 握手失败")?;
    Ok(stream)
}

fn insert_header(headers: &mut http::HeaderMap, name: &str, value: &str) {
    if let (Ok(name), Ok(value)) = (
        http::HeaderName::from_bytes(name.as_bytes()),
        http::HeaderValue::from_str(value),
    ) {
        headers.insert(name, value);
    }
}

async fn dial_io(url: &Url, proxy: &str) -> Result<BoxIo> {
    let host = url
        .host_str()
        .context("上游 WebSocket 缺少主机名")?
        .to_string();
    let port = url
        .port_or_known_default()
        .context("上游 WebSocket 缺少端口")?;
    let io = if proxy.trim().is_empty() {
        BoxIo::new(TcpStream::connect((host.as_str(), port)).await?)
    } else {
        connect_via_proxy(proxy.trim(), &host, port).await?
    };
    if url.scheme() == "wss" {
        tls_wrap(&host, io).await
    } else {
        Ok(io)
    }
}

async fn tls_wrap(host: &str, io: BoxIo) -> Result<BoxIo> {
    let server_name = rustls::pki_types::ServerName::try_from(host.to_owned())
        .map_err(|_| anyhow!("无效的上游主机名"))?;
    let tls = tls_connector()
        .connect(server_name, io)
        .await
        .context("上游 TLS 握手失败")?;
    Ok(BoxIo::new(tls))
}

fn tls_connector() -> &'static tokio_rustls::TlsConnector {
    static CONNECTOR: OnceLock<tokio_rustls::TlsConnector> = OnceLock::new();
    CONNECTOR.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(config))
    })
}

async fn connect_via_proxy(proxy: &str, host: &str, port: u16) -> Result<BoxIo> {
    let url = Url::parse(proxy).context("出站代理地址无效")?;
    match url.scheme() {
        "http" => connect_via_http_proxy(&url, host, port).await,
        "socks5" | "socks5h" => connect_via_socks5(&url, host, port).await,
        "socks4" | "socks4a" => connect_via_socks4(&url, host, port).await,
        "https" => Err(anyhow!(
            "HTTPS 代理暂不支持 WebSocket 隧道，将回退 HTTP"
        )),
        other => Err(anyhow!("不支持的 WebSocket 代理协议: {other}")),
    }
}

async fn connect_via_http_proxy(proxy: &Url, host: &str, port: u16) -> Result<BoxIo> {
    let proxy_host = proxy.host_str().context("HTTP 代理缺少主机名")?;
    let proxy_port = proxy.port_or_known_default().unwrap_or(80);
    let mut stream = TcpStream::connect((proxy_host, proxy_port))
        .await
        .context("连接 HTTP 代理失败")?;
    let target = host_port(host, port);
    let mut request = format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n");
    if !proxy.username().is_empty() {
        let username = proxy.username();
        let password = proxy.password().unwrap_or("");
        let token = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{username}:{password}"),
        );
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .context("发送 CONNECT 失败")?;
    let (status, leftover) = read_http_head(&mut stream).await?;
    if !status_is_ok(&status) {
        anyhow::bail!("CONNECT 隧道失败: {}", status.trim());
    }
    Ok(BoxIo::prefixed(stream, leftover))
}

async fn connect_via_socks5(proxy: &Url, host: &str, port: u16) -> Result<BoxIo> {
    let proxy_addr = proxy_socket(proxy, 1080)?;
    let target = (host.to_string(), port);
    let stream = if proxy.username().is_empty() {
        tokio_socks::tcp::Socks5Stream::connect(proxy_addr, target)
            .await
            .context("SOCKS5 连接失败")?
    } else {
        tokio_socks::tcp::Socks5Stream::connect_with_password(
            proxy_addr,
            target,
            proxy.username(),
            proxy.password().unwrap_or(""),
        )
        .await
        .context("SOCKS5 认证连接失败")?
    };
    Ok(BoxIo::new(stream))
}

async fn connect_via_socks4(proxy: &Url, host: &str, port: u16) -> Result<BoxIo> {
    let proxy_addr = proxy_socket(proxy, 1080)?;
    let target = (host.to_string(), port);
    let userid = proxy.username();
    let stream = if userid.is_empty() {
        tokio_socks::tcp::Socks4Stream::connect(proxy_addr, target)
            .await
            .context("SOCKS4 连接失败")?
    } else {
        tokio_socks::tcp::Socks4Stream::connect_with_userid(proxy_addr, target, userid)
            .await
            .context("SOCKS4 认证连接失败")?
    };
    Ok(BoxIo::new(stream))
}

fn proxy_socket(proxy: &Url, default_port: u16) -> Result<std::net::SocketAddr> {
    let host = proxy.host_str().context("代理缺少主机名")?;
    let port = proxy.port_or_known_default().unwrap_or(default_port);
    let mut addrs = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
        .with_context(|| format!("无法解析代理地址 {host}:{port}"))?;
    addrs.next().context("代理地址没有解析结果")
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn status_is_ok(status: &str) -> bool {
    status
        .split_whitespace()
        .nth(1)
        .is_some_and(|code| code == "200")
}

async fn read_http_head(stream: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 512];
    loop {
        let read = stream.read(&mut tmp).await.context("读取代理响应失败")?;
        if read == 0 {
            anyhow::bail!("代理在 CONNECT 响应前关闭了连接");
        }
        buf.extend_from_slice(&tmp[..read]);
        if let Some(end) = buf.windows(4).position(|window| window == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&buf[..end]).to_string();
            let status = header.lines().next().unwrap_or("").to_string();
            let leftover = buf[end + 4..].to_vec();
            return Ok((status, leftover));
        }
        if buf.len() > 8192 {
            anyhow::bail!("代理 CONNECT 响应过长");
        }
    }
}

trait AsyncReadWrite: AsyncRead + AsyncWrite + Send {}
impl<T: AsyncRead + AsyncWrite + Send> AsyncReadWrite for T {}

pub struct BoxIo {
    inner: Pin<Box<dyn AsyncReadWrite>>,
}

impl BoxIo {
    fn new<T>(stream: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            inner: Box::pin(stream),
        }
    }

    fn prefixed(stream: TcpStream, leftover: Vec<u8>) -> Self {
        if leftover.is_empty() {
            Self::new(stream)
        } else {
            Self::new(PrefixedTcp {
                rest: leftover,
                pos: 0,
                stream,
            })
        }
    }
}

impl AsyncRead for BoxIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        self.get_mut().inner.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for BoxIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.get_mut().inner.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut().inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.get_mut().inner.as_mut().poll_shutdown(cx)
    }
}

struct PrefixedTcp {
    rest: Vec<u8>,
    pos: usize,
    stream: TcpStream,
}

impl AsyncRead for PrefixedTcp {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.pos < this.rest.len() {
            let n = (this.rest.len() - this.pos).min(buf.remaining());
            buf.put_slice(&this.rest[this.pos..this.pos + n]);
            this.pos += n;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedTcp {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use serde_json::json;

    #[test]
    fn connection_expires_before_the_upstream_hour() {
        assert!(connection_fresh(Duration::from_secs(50 * 60)));
        assert!(!connection_fresh(Duration::from_secs(56 * 60)));
    }

    #[tokio::test]
    async fn round_trip_against_local_websocket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let message = ws.next().await.unwrap().unwrap();
            let text = message.into_text().unwrap();
            assert!(text.contains("response.create"));
            ws.send(Message::text(r#"{"type":"response.created"}"#))
                .await
                .unwrap();
            ws.send(Message::text(
                r#"{"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":2}}}"#,
            ))
            .await
            .unwrap();
        });
        let pool = WsUpstreamPool::new();
        let mut rx = pool
            .open_turn(
                WsDial {
                    url: format!("ws://{addr}/responses"),
                    proxy: String::new(),
                    authorization: "Bearer test".into(),
                    account_id: "acct".into(),
                    extra_headers: vec![("originator".into(), "codex_cli_rs".into())],
                },
                json!({"type":"response.create","model":"m"}),
            )
            .await
            .unwrap();
        let first = rx.recv().await.unwrap().unwrap();
        assert!(first.contains("response.created"));
        let second = rx.recv().await.unwrap().unwrap();
        assert!(ws_bridge::is_terminal_event(&second));
        assert!(rx.recv().await.is_none());
        let snapshot = pool.snapshot().await;
        assert!(snapshot.connected);
        assert!(snapshot.connected_at.is_some());
    }

    #[tokio::test]
    async fn http_connect_tunnel_reaches_websocket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 512];
            loop {
                let n = stream.read(&mut tmp).await.unwrap();
                buf.extend_from_slice(&tmp[..n]);
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let head = String::from_utf8_lossy(&buf);
            assert!(head.starts_with("CONNECT chatgpt.com:443 "));
            assert!(head.contains("Proxy-Authorization: Basic"));
            stream
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.send(Message::text(r#"{"type":"response.completed"}"#))
                .await
                .unwrap();
        });
        let pool = WsUpstreamPool::new();
        let mut rx = pool
            .open_turn(
                WsDial {
                    url: "ws://chatgpt.com:443/backend-api/codex/responses".into(),
                    proxy: format!("http://user:pass@{addr}"),
                    authorization: String::new(),
                    account_id: String::new(),
                    extra_headers: vec![],
                },
                json!({"type":"response.create"}),
            )
            .await
            .unwrap();
        let event = rx.recv().await.unwrap().unwrap();
        assert!(ws_bridge::is_terminal_event(&event));
    }

    #[tokio::test]
    async fn connection_limit_is_resent_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(first).await.unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.send(Message::text(
                r#"{"type":"error","error":{"code":"websocket_connection_limit_reached"}}"#,
            ))
            .await
            .unwrap();
            let (second, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(second).await.unwrap();
            let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(text.contains("response.create"));
            ws.send(Message::text(r#"{"type":"response.completed"}"#))
                .await
                .unwrap();
        });
        let pool = WsUpstreamPool::new();
        let mut rx = pool
            .open_turn(
                WsDial {
                    url: format!("ws://{addr}/responses"),
                    proxy: String::new(),
                    authorization: String::new(),
                    account_id: String::new(),
                    extra_headers: vec![],
                },
                json!({"type":"response.create","model":"m"}),
            )
            .await
            .unwrap();
        let event = rx.recv().await.unwrap().unwrap();
        assert!(event.contains("response.completed"));
        assert!(!event.contains("connection_limit"));
    }
}
