//! 到 Codex 上游的 WebSocket 连接池。
//!
//! 与官方客户端一样，一条连接一次只跑一轮：并发请求各用一条连接，最多
//! `MAX_CONNECTIONS` 条。带 `previous_response_id` 的续跑回到产生该响应的连接，
//! 上游只在同一条连接上认得它。
//!
//! 后台反复调用 [`WsUpstreamPool::maintain`]：预先握手一条空闲连接待用，定期
//! ping 保活，快到期前换新，账号或线路变化后立即重新预热，多出来的空闲连接
//! 闲置一段时间后关闭。空闲时被对端断开的连接，在下一轮发送前后自动重连重发。
//! 握手失败后退避一段时间，期间 HTTP 请求直接走 SSE，不再逐个尝试 WebSocket。
//!
//! Responses WebSocket 握手启用上游 Codex 使用的 `permessage-deflate`。

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex, Notify, OwnedMutexGuard};
use tokio_tungstenite::tungstenite::extensions::{
    compression::deflate::DeflateConfig, ExtensionsConfig,
};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tokio_tungstenite::WebSocketStream;
use url::Url;

use crate::outbound;
use crate::ws_bridge;

/// 上游约一小时断开一条连接，留出余量。
const MAX_AGE: Duration = Duration::from_secs(55 * 60);
/// 空闲连接用到这个岁数就提前换新，避免请求撞上到期。
const REFRESH_AGE: Duration = Duration::from_secs(50 * 60);
const MAX_CONNECTIONS: usize = 8;
/// 预热那一条之外的空闲连接，闲置这么久后关闭。
const SPARE_IDLE: Duration = Duration::from_secs(10 * 60);
const PING_TIMEOUT: Duration = Duration::from_secs(10);
const BACKOFF_START: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(120);
/// 每条连接记住它产生的最近这么多个响应 ID，用来把续跑送回原连接。
const REMEMBERED_RESPONSES: usize = 256;
const OPENAI_BETA: &str = "responses_websockets=2026-02-06";

fn websocket_config() -> WebSocketConfig {
    let mut extensions = ExtensionsConfig::default();
    extensions.permessage_deflate = Some(DeflateConfig::default());

    let mut config = WebSocketConfig::default();
    config.extensions = extensions;
    config
}

#[derive(Clone)]
pub struct WsDial {
    pub url: String,
    pub proxy: String,
    pub authorization: String,
    pub account_id: String,
    pub extra_headers: Vec<(String, String)>,
    /// Client identity headers must select their own pooled connection when
    /// virtual-device simulation is disabled.
    pub preserve_client_identity: bool,
}

impl WsDial {
    fn key(&self) -> String {
        // Turn metadata is deliberately excluded: it describes one request,
        // while a WebSocket belongs to a window/thread scope.  Keep the rest
        // deterministic so header ordering or casing cannot create a second
        // socket for the same scope.
        let mut headers: Vec<_> = self
            .extra_headers
            .iter()
            .filter_map(|(name, value)| {
                let name = name.trim().to_ascii_lowercase();
                let value = value.trim();
                if name.is_empty() || value.is_empty() || is_turn_only_header(&name) {
                    return None;
                }
                Some((name, value.to_string()))
            })
            .collect();
        headers.sort_unstable();
        let base = format!(
            "{}\n{}\n{}\n{}\n{}",
            self.url.trim(),
            self.proxy.trim(),
            self.authorization.trim(),
            self.account_id.trim(),
            headers
                .into_iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        base
    }
}

fn is_turn_only_header(name: &str) -> bool {
    matches!(
        name,
        "x-codex-turn-metadata" | "x-codex-turn-id" | "turn-id" | "x-turn-id"
    )
}

#[derive(Clone, Default)]
pub struct WsUpstreamPool {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    slots: std::sync::Mutex<Vec<Arc<Slot>>>,
    sticky_session: std::sync::Mutex<Option<String>>,
    backoff: std::sync::Mutex<Backoff>,
    /// Bumped by `invalidate`; connections from an older generation are
    /// replaced before their next turn.
    generation: AtomicU64,
    changed: Notify,
    /// Model of the latest turn, so a warm-up handshake carries the same
    /// routing hint as real requests.
    last_model: std::sync::Mutex<String>,
}

#[derive(Default)]
struct Backoff {
    until: Option<Instant>,
    delay: Duration,
}

#[derive(Default)]
struct Slot {
    conn: Arc<Mutex<Option<Live>>>,
    /// Response ids produced on the current connection, newest last.
    responses: std::sync::Mutex<VecDeque<String>>,
    response_scope: std::sync::Mutex<Option<String>>,
}

impl Slot {
    fn owns(&self, key: &str, response_id: &str) -> bool {
        let scope = lock(&self.response_scope);
        if scope.as_deref() != Some(key) {
            return false;
        }
        lock(&self.responses).iter().any(|id| id == response_id)
    }

    fn remember(&self, key: &str, response_id: &str) {
        let mut scope = lock(&self.response_scope);
        if scope.as_deref() != Some(key) {
            *scope = Some(key.to_string());
            lock(&self.responses).clear();
        }
        let mut responses = lock(&self.responses);
        if responses.back().is_some_and(|last| last == response_id) {
            return;
        }
        responses.push_back(response_id.to_string());
        while responses.len() > REMEMBERED_RESPONSES {
            responses.pop_front();
        }
    }

    fn forget(&self) {
        *lock(&self.response_scope) = None;
        lock(&self.responses).clear();
    }
}

type ConnGuard = OwnedMutexGuard<Option<Live>>;

struct Live {
    ws: WebSocketStream<BoxIo>,
    connected_at: Instant,
    idle_since: Instant,
    key: String,
    generation: u64,
    /// Handshake response headers (`openai-model`, safety buffering, …).
    headers: http::HeaderMap,
}

enum Read {
    Event(String),
    Fail(String),
}

fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl WsUpstreamPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops every connection (in-flight turns finish first) and re-warms:
    /// called when the account, device or outbound line changes.
    pub async fn invalidate(&self) {
        self.shared.generation.fetch_add(1, Ordering::SeqCst);
        *lock(&self.shared.sticky_session) = None;
        *lock(&self.shared.backoff) = Backoff::default();
        let slots = lock(&self.shared.slots).clone();
        for slot in slots {
            if let Ok(mut conn) = slot.conn.clone().try_lock_owned() {
                *conn = None;
            }
            slot.forget();
        }
        self.shared.changed.notify_one();
    }

    pub fn last_model(&self) -> String {
        lock(&self.shared.last_model).clone()
    }

    /// Resolves when `invalidate` asked for a re-warm.
    pub async fn changed(&self) {
        self.shared.changed.notified().await;
    }

    /// False while backing off after a failed handshake: HTTP requests then
    /// go straight to SSE.
    pub fn available(&self) -> bool {
        lock(&self.shared.backoff)
            .until
            .is_none_or(|until| Instant::now() >= until)
    }

    /// Number of open connections, for tests and diagnostics.
    pub fn open_connections(&self) -> usize {
        lock(&self.shared.slots)
            .iter()
            .filter(|slot| {
                slot.conn
                    .clone()
                    .try_lock_owned()
                    .map_or(true, |conn| conn.is_some())
            })
            .count()
    }

    /// `{session}` 在连接池的生命周期内保持不变。调用方已解析好的地址原样返回。
    pub async fn resolve_proxy(&self, template: &str, bound: Option<&str>) -> Result<String> {
        let template = template.trim();
        if !outbound::has_session_placeholder(template) {
            return Ok(outbound::dial_proxy_for_client(template));
        }
        if let Some(session) = bound.map(str::trim).filter(|value| !value.is_empty()) {
            return Ok(outbound::dial_proxy_for_client(
                &outbound::apply_bound_session(template, Some(session))?,
            ));
        }
        let session = lock(&self.shared.sticky_session)
            .get_or_insert_with(outbound::generate_proxy_session)
            .clone();
        Ok(outbound::dial_proxy_for_client(
            &outbound::replace_session_placeholder(template, &session),
        ))
    }

    /// Sends one turn and streams its events; also returns the handshake
    /// headers of the connection that carried it.
    pub async fn open_turn(
        &self,
        dial: WsDial,
        payload: Value,
    ) -> Result<(mpsc::Receiver<Result<String, String>>, http::HeaderMap)> {
        if let Some(model) = payload
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|model| !model.is_empty())
        {
            *lock(&self.shared.last_model) = model.to_string();
        }
        let (slot, mut conn) = self.pick_slot(&dial, &payload).await;
        let had_connection = conn.is_some();
        self.ensure(&mut conn, &slot, &dial).await?;
        if send_frame(&mut conn, &payload).await.is_err() {
            // An idle connection the upstream already closed: redial once.
            *conn = None;
            slot.forget();
            if !had_connection {
                anyhow::bail!("发送上游 WebSocket 帧失败");
            }
            self.ensure(&mut conn, &slot, &dial).await?;
            send_frame(&mut conn, &payload).await.inspect_err(|_| {
                *conn = None;
            })?;
        }
        let headers = conn
            .as_ref()
            .map(|live| live.headers.clone())
            .unwrap_or_default();
        let (tx, rx) = mpsc::channel(32);
        let pool = self.clone();
        tokio::spawn(async move {
            pool.drive(slot, conn, dial, payload, tx).await;
        });
        Ok((rx, headers))
    }

    /// Keeps one idle connection for `dial` warm and healthy; call it
    /// periodically. Busy connections are left alone.
    pub async fn maintain(&self, dial: &WsDial) {
        if !self.available() {
            return;
        }
        let key = dial.key();
        let generation = self.generation();
        let slots = lock(&self.shared.slots).clone();
        let mut warm = false;
        for slot in &slots {
            let Ok(mut conn) = slot.conn.clone().try_lock_owned() else {
                continue;
            };
            let Some(live) = conn.as_mut() else {
                continue;
            };
            // A warm run must not evict another healthy window/thread merely
            // because its key differs. Ping other scopes as well so an idle
            // window does not silently expire while a different one is active.
            if live.key != key {
                let expired = live.generation != generation
                    || live.connected_at.elapsed() >= MAX_AGE
                    || (warm && live.idle_since.elapsed() >= SPARE_IDLE);
                if expired || ping(live).await.is_err() {
                    close(&mut conn).await;
                    slot.forget();
                }
                continue;
            }
            let outdated =
                live.generation != generation || live.connected_at.elapsed() >= REFRESH_AGE;
            let spare = warm && live.idle_since.elapsed() >= SPARE_IDLE;
            if outdated || spare || ping(live).await.is_err() {
                close(&mut conn).await;
                slot.forget();
                continue;
            }
            warm = true;
        }
        if warm {
            return;
        }
        let Some((slot, mut conn)) = self.idle_slot() else {
            return;
        };
        if let Err(err) = self.ensure(&mut conn, &slot, dial).await {
            eprintln!("[ws] 预热上游 WebSocket 失败: {err:#}");
        }
    }

    fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::SeqCst)
    }

    /// An idle slot without a connection, adding one when there is room.
    fn idle_slot(&self) -> Option<(Arc<Slot>, ConnGuard)> {
        let mut slots = lock(&self.shared.slots);
        for slot in slots.iter() {
            if let Ok(conn) = slot.conn.clone().try_lock_owned() {
                if conn.is_none() {
                    return Some((slot.clone(), conn));
                }
            }
        }
        if slots.len() >= MAX_CONNECTIONS {
            return None;
        }
        let slot = Arc::new(Slot::default());
        slots.push(slot.clone());
        let conn = slot.conn.clone().try_lock_owned().ok()?;
        Some((slot, conn))
    }

    /// The connection for a turn: the one in this scope that produced its
    /// `previous_response_id`, else a matching idle connection, then an empty
    /// slot/new connection, else an idle connection from another scope when
    /// the pool is full.
    async fn pick_slot(&self, dial: &WsDial, payload: &Value) -> (Arc<Slot>, ConnGuard) {
        let key = dial.key();
        let previous = payload
            .get("previous_response_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let slots = lock(&self.shared.slots).clone();
        if let Some(owner) = previous.and_then(|id| slots.iter().find(|slot| slot.owns(&key, id))) {
            let conn = owner.conn.clone().lock_owned().await;
            return (owner.clone(), conn);
        }
        let mut matching = None;
        let mut empty = None;
        let mut other_idle = None;
        for slot in &slots {
            if let Ok(conn) = slot.conn.clone().try_lock_owned() {
                if conn.as_ref().is_some_and(|live| live.key == key) {
                    if matching.is_none() {
                        matching = Some((slot.clone(), conn));
                    }
                } else if conn.is_some() && other_idle.is_none() {
                    other_idle = Some((slot.clone(), conn));
                } else if conn.is_none() && empty.is_none() {
                    empty = Some((slot.clone(), conn));
                }
            }
        }
        if let Some(found) = matching {
            return found;
        }
        if let Some(found) = empty {
            return found;
        }
        if let Some(found) = self.idle_slot() {
            return found;
        }
        if let Some(found) = other_idle {
            return found;
        }
        let waiting = slots
            .iter()
            .map(|slot| Box::pin(slot.conn.clone().lock_owned()));
        let (conn, index, _) = futures_util::future::select_all(waiting).await;
        (slots[index].clone(), conn)
    }

    async fn ensure(&self, conn: &mut ConnGuard, slot: &Slot, dial: &WsDial) -> Result<()> {
        let key = dial.key();
        let generation = self.generation();
        let fresh = conn.as_ref().is_some_and(|live| {
            live.key == key
                && live.generation == generation
                && connection_fresh(live.connected_at.elapsed())
        });
        if fresh {
            return Ok(());
        }
        close(conn).await;
        slot.forget();
        match connect_upstream(dial).await {
            Ok((ws, headers)) => {
                *lock(&self.shared.backoff) = Backoff::default();
                let now = Instant::now();
                **conn = Some(Live {
                    ws,
                    connected_at: now,
                    idle_since: now,
                    key,
                    generation,
                    headers,
                });
                Ok(())
            }
            Err(err) => {
                let mut backoff = lock(&self.shared.backoff);
                backoff.delay = if backoff.delay.is_zero() {
                    BACKOFF_START
                } else {
                    (backoff.delay * 2).min(BACKOFF_MAX)
                };
                backoff.until = Some(Instant::now() + backoff.delay);
                Err(err)
            }
        }
    }

    async fn drive(
        &self,
        slot: Arc<Slot>,
        mut conn: ConnGuard,
        dial: WsDial,
        mut payload: Value,
        tx: mpsc::Sender<Result<String, String>>,
    ) {
        let mut saw_event = false;
        let mut retried_limit = false;
        let mut retried_missing = false;
        let mut retried_closed = false;
        loop {
            match read_one(&mut conn, saw_event).await {
                Read::Event(text) => {
                    if !saw_event
                        && !retried_limit
                        && ws_bridge::ws_error_code(&text).as_deref()
                            == Some("websocket_connection_limit_reached")
                    {
                        retried_limit = true;
                        *conn = None;
                        slot.forget();
                        *lock(&self.shared.sticky_session) = None;
                        if self.ensure(&mut conn, &slot, &dial).await.is_err()
                            || send_frame(&mut conn, &payload).await.is_err()
                        {
                            *conn = None;
                            let _ = tx
                                .send(Err("上游 WebSocket 达到连接时限，重连失败".into()))
                                .await;
                            break;
                        }
                        continue;
                    }
                    if !saw_event
                        && !retried_missing
                        && ws_bridge::is_previous_response_error(&text)
                    {
                        retried_missing = true;
                        ws_bridge::strip_previous_response_id(&mut payload);
                        if send_frame(&mut conn, &payload).await.is_err() {
                            *conn = None;
                            slot.forget();
                            let _ = tx.send(Err("链式响应已失效，重发失败".into())).await;
                            break;
                        }
                        continue;
                    }
                    if let Some(id) = response_id(&text) {
                        slot.remember(&dial.key(), &id);
                    }
                    let terminal = ws_bridge::is_terminal_event(&text);
                    saw_event = true;
                    if tx.send(Ok(text)).await.is_err() {
                        *conn = None;
                        slot.forget();
                        break;
                    }
                    if terminal {
                        break;
                    }
                }
                Read::Fail(message) => {
                    *conn = None;
                    slot.forget();
                    // Closed before answering at all: the connection died
                    // while idle, so redial and resend once.
                    if !saw_event && !retried_closed && message == CLOSED {
                        retried_closed = true;
                        if self.ensure(&mut conn, &slot, &dial).await.is_ok()
                            && send_frame(&mut conn, &payload).await.is_ok()
                        {
                            continue;
                        }
                        *conn = None;
                    }
                    let _ = tx.send(Err(message)).await;
                    break;
                }
            }
        }
        if let Some(live) = conn.as_mut() {
            live.idle_since = Instant::now();
        }
    }
}

const CLOSED: &str = "上游 WebSocket 连接已关闭";

fn response_id(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    value
        .pointer("/response/id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
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

async fn close(conn: &mut Option<Live>) {
    if let Some(mut live) = conn.take() {
        let _ = tokio::time::timeout(Duration::from_secs(2), live.ws.close(None)).await;
    }
}

/// Pings an idle connection and waits for the pong, answering the
/// upstream's own pings meanwhile.
async fn ping(live: &mut Live) -> Result<()> {
    live.ws
        .send(Message::Ping(Vec::new().into()))
        .await
        .context("发送 ping 失败")?;
    tokio::time::timeout(PING_TIMEOUT, async {
        loop {
            match live.ws.next().await {
                Some(Ok(Message::Pong(_))) => return Ok(()),
                Some(Ok(Message::Ping(payload))) => {
                    live.ws.send(Message::Pong(payload)).await?;
                }
                Some(Ok(Message::Close(_))) | None => anyhow::bail!("连接已关闭"),
                Some(Ok(_)) => {}
                Some(Err(err)) => return Err(err.into()),
            }
        }
    })
    .await
    .context("ping 超时")?
}

async fn send_frame(conn: &mut Option<Live>, payload: &Value) -> Result<()> {
    let live = conn.as_mut().context("上游 WebSocket 未连接")?;
    let text = serde_json::to_string(payload).context("无法序列化 WebSocket 请求")?;
    live.ws
        .send(Message::text(text))
        .await
        .context("发送上游 WebSocket 帧失败")?;
    Ok(())
}

async fn read_one(conn: &mut Option<Live>, saw_event: bool) -> Read {
    let idle = idle_timeout(saw_event);
    loop {
        let Some(live) = conn.as_mut() else {
            return Read::Fail("上游 WebSocket 未连接".into());
        };
        if !connection_fresh(live.connected_at.elapsed()) {
            return Read::Fail("上游 WebSocket 连接已到期".into());
        }
        let message = tokio::time::timeout(idle, live.ws.next()).await;
        match message {
            Err(_) => {
                return Read::Fail(if saw_event {
                    "上游 WebSocket 在事件之间静默超时".into()
                } else {
                    "上游 WebSocket 在首个事件前静默超时".into()
                });
            }
            Ok(None) | Ok(Some(Ok(Message::Close(_)))) => return Read::Fail(CLOSED.into()),
            Ok(Some(Err(err))) => {
                return Read::Fail(format!("上游 WebSocket 读取失败: {err}"));
            }
            Ok(Some(Ok(Message::Ping(payload)))) => {
                if live.ws.send(Message::Pong(payload)).await.is_err() {
                    return Read::Fail("上游 WebSocket Pong 失败".into());
                }
            }
            Ok(Some(Ok(Message::Text(text)))) => return Read::Event(text.to_string()),
            Ok(Some(Ok(Message::Pong(_) | Message::Binary(_) | Message::Frame(_)))) => {}
        }
    }
}

async fn connect_upstream(dial: &WsDial) -> Result<(WebSocketStream<BoxIo>, http::HeaderMap)> {
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
    let (stream, response) =
        tokio_tungstenite::client_async_with_config(request, io, Some(websocket_config()))
            .await
            .context("上游 WebSocket 握手失败")?;
    Ok((stream, response.headers().clone()))
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
        let stream = TcpStream::connect((host.as_str(), port)).await?;
        stream.set_nodelay(true).ok();
        BoxIo::new(stream)
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
    // Keep the connector and root store shared across pooled sockets, while
    // deriving the actual config in the common TLS module so HTTP/WS updates
    // cannot accidentally select different providers or root sets.
    static CONNECTOR: std::sync::OnceLock<tokio_rustls::TlsConnector> = std::sync::OnceLock::new();
    CONNECTOR
        .get_or_init(|| tokio_rustls::TlsConnector::from(crate::tls::websocket_client_config()))
}

async fn connect_via_proxy(proxy: &str, host: &str, port: u16) -> Result<BoxIo> {
    let url = Url::parse(proxy).context("出站代理地址无效")?;
    match url.scheme() {
        "http" => connect_via_http_proxy(&url, host, port).await,
        "socks5" | "socks5h" => connect_via_socks5(&url, host, port).await,
        "socks4" | "socks4a" => connect_via_socks4(&url, host, port).await,
        "https" => Err(anyhow!("HTTPS 代理暂不支持 WebSocket 隧道，将回退 HTTP")),
        other => Err(anyhow!("不支持的 WebSocket 代理协议: {other}")),
    }
}

async fn connect_via_http_proxy(proxy: &Url, host: &str, port: u16) -> Result<BoxIo> {
    let proxy_host = proxy.host_str().context("HTTP 代理缺少主机名")?;
    let proxy_port = proxy.port_or_known_default().unwrap_or(80);
    let mut stream = TcpStream::connect((proxy_host, proxy_port))
        .await
        .context("连接 HTTP 代理失败")?;
    stream.set_nodelay(true).ok();
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
    stream.set_nodelay(true).ok();
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
    stream.set_nodelay(true).ok();
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

    #[test]
    fn websocket_config_enables_permessage_deflate() {
        assert!(websocket_config().extensions.permessage_deflate.is_some());
    }

    #[test]
    fn scope_key_is_ordered_and_ignores_turn_metadata() {
        let mut first = local_dial("127.0.0.1:1".parse().unwrap());
        first.extra_headers = vec![
            ("X-Codex-Turn-Metadata".into(), "turn-a".into()),
            ("X-Codex-Window-Id".into(), "window-1".into()),
            ("Thread-Id".into(), "thread-1".into()),
        ];
        let mut second = first.clone();
        second.extra_headers.reverse();
        second
            .extra_headers
            .iter_mut()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-codex-turn-metadata"))
            .unwrap()
            .1 = "turn-b".into();
        assert_eq!(first.key(), second.key());
    }

    #[tokio::test]
    #[allow(clippy::result_large_err)] // tungstenite's handshake callback signature
    async fn round_trip_against_local_websocket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async_with_config(
                stream,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request,
                 mut response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                    assert_eq!(
                        request
                            .headers()
                            .get("sec-websocket-extensions")
                            .and_then(|value| value.to_str().ok()),
                        Some("permessage-deflate; client_max_window_bits")
                    );
                    response
                        .headers_mut()
                        .insert("openai-model", http::HeaderValue::from_static("gpt-5.6-luna"));
                    Ok(response)
                },
                Some(websocket_config()),
            )
            .await
            .unwrap();
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
        let (mut rx, handshake) = pool
            .open_turn(
                WsDial {
                    url: format!("ws://{addr}/responses"),
                    proxy: String::new(),
                    authorization: "Bearer test".into(),
                    account_id: "acct".into(),
                    extra_headers: vec![("originator".into(), "codex_cli_rs".into())],
                    preserve_client_identity: false,
                },
                json!({"type":"response.create","model":"m"}),
            )
            .await
            .unwrap();
        // The handshake headers carry the serving model for downgrade checks.
        assert_eq!(handshake.get("openai-model").unwrap(), "gpt-5.6-luna");
        let first = rx.recv().await.unwrap().unwrap();
        assert!(first.contains("response.created"));
        let second = rx.recv().await.unwrap().unwrap();
        assert!(ws_bridge::is_terminal_event(&second));
        assert!(rx.recv().await.is_none());
        assert_eq!(pool.open_connections(), 1);
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
            let mut ws =
                tokio_tungstenite::accept_async_with_config(stream, Some(websocket_config()))
                    .await
                    .unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.send(Message::text(r#"{"type":"response.completed"}"#))
                .await
                .unwrap();
        });
        let pool = WsUpstreamPool::new();
        let (mut rx, _) = pool
            .open_turn(
                WsDial {
                    url: "ws://chatgpt.com:443/backend-api/codex/responses".into(),
                    proxy: format!("http://user:pass@{addr}"),
                    authorization: String::new(),
                    account_id: String::new(),
                    extra_headers: vec![],
                    preserve_client_identity: false,
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
            let mut ws =
                tokio_tungstenite::accept_async_with_config(first, Some(websocket_config()))
                    .await
                    .unwrap();
            let _ = ws.next().await.unwrap().unwrap();
            ws.send(Message::text(
                r#"{"type":"error","error":{"code":"websocket_connection_limit_reached"}}"#,
            ))
            .await
            .unwrap();
            let (second, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_async_with_config(second, Some(websocket_config()))
                    .await
                    .unwrap();
            let text = ws.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(text.contains("response.create"));
            ws.send(Message::text(r#"{"type":"response.completed"}"#))
                .await
                .unwrap();
        });
        let pool = WsUpstreamPool::new();
        let (mut rx, _) = pool
            .open_turn(
                WsDial {
                    url: format!("ws://{addr}/responses"),
                    proxy: String::new(),
                    authorization: String::new(),
                    account_id: String::new(),
                    extra_headers: vec![],
                    preserve_client_identity: false,
                },
                json!({"type":"response.create","model":"m"}),
            )
            .await
            .unwrap();
        let event = rx.recv().await.unwrap().unwrap();
        assert!(event.contains("response.completed"));
        assert!(!event.contains("connection_limit"));
    }

    #[tokio::test]
    async fn invalid_previous_response_id_is_resent_without_chain_handle() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dial = local_dial(listener.local_addr().unwrap());
        let pool = WsUpstreamPool::new();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws =
                tokio_tungstenite::accept_async_with_config(stream, Some(websocket_config()))
                    .await
                    .unwrap();
            let first = ws.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(first.contains("previous_response_id"));
            ws.send(Message::text(
                r#"{"type":"error","error":{"code":"invalid_request_error","message":"Invalid `previous_response_id`"}}"#,
            ))
            .await
            .unwrap();
            let retry = ws.next().await.unwrap().unwrap().into_text().unwrap();
            assert!(!retry.contains("previous_response_id"));
            ws.send(Message::text(
                r#"{"type":"response.completed","response":{"id":"resp_retry"}}"#,
            ))
            .await
            .unwrap();
        });

        let (mut rx, _) = pool
            .open_turn(
                dial,
                json!({
                    "type":"response.create",
                    "model":"m",
                    "previous_response_id":"resp_stale",
                    "input":[{"type":"function_call_output","call_id":"call_1","output":"ok"}]
                }),
            )
            .await
            .unwrap();
        let event = rx.recv().await.unwrap().unwrap();
        assert_eq!(response_id(&event).as_deref(), Some("resp_retry"));
    }

    fn local_dial(addr: std::net::SocketAddr) -> WsDial {
        WsDial {
            url: format!("ws://{addr}/responses"),
            proxy: String::new(),
            authorization: "Bearer test".into(),
            account_id: "acct".into(),
            extra_headers: vec![],
            preserve_client_identity: false,
        }
    }

    fn scoped_dial(
        addr: std::net::SocketAddr,
        window: &str,
        thread: &str,
        turn_metadata: &str,
    ) -> WsDial {
        let mut dial = local_dial(addr);
        dial.extra_headers = vec![
            ("user-agent".into(), "codex/0.1".into()),
            ("x-codex-installation-id".into(), "install-1".into()),
            ("x-codex-window-id".into(), window.into()),
            ("session-id".into(), "session-1".into()),
            ("thread-id".into(), thread.into()),
            ("x-client-request-id".into(), thread.into()),
            ("x-codex-turn-metadata".into(), turn_metadata.into()),
        ];
        dial
    }

    /// A local upstream: every connection answers each `response.create`
    /// with `response.completed` whose id names the connection and turn.
    /// Frames with `"model":"slow"` wait for `release` first; a connection
    /// numbered in `close_after_one` closes after its first turn.
    fn spawn_upstream(
        listener: tokio::net::TcpListener,
        release: Arc<Notify>,
        close_after_one: Option<usize>,
    ) -> Arc<std::sync::atomic::AtomicUsize> {
        let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let index = counter.fetch_add(1, Ordering::SeqCst);
                let release = release.clone();
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async_with_config(
                        stream,
                        Some(websocket_config()),
                    )
                    .await
                    .unwrap();
                    let mut turn = 0;
                    while let Some(Ok(message)) = ws.next().await {
                        match message {
                            Message::Ping(payload) => {
                                let _ = ws.send(Message::Pong(payload)).await;
                            }
                            Message::Text(text) => {
                                turn += 1;
                                if text.contains("\"model\":\"slow\"") {
                                    release.notified().await;
                                }
                                let done = format!(
                                    r#"{{"type":"response.completed","response":{{"id":"resp_{index}_{turn}"}}}}"#
                                );
                                if ws.send(Message::text(done)).await.is_err() {
                                    return;
                                }
                                if close_after_one == Some(index) {
                                    let _ = ws.close(None).await;
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
        });
        accepted
    }

    async fn completed_id(pool: &WsUpstreamPool, dial: WsDial, payload: Value) -> String {
        let (mut rx, _) = pool.open_turn(dial, payload).await.unwrap();
        let event = rx.recv().await.unwrap().unwrap();
        response_id(&event).unwrap()
    }

    #[tokio::test]
    async fn concurrent_turns_do_not_wait_for_each_other() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dial = local_dial(listener.local_addr().unwrap());
        let release = Arc::new(Notify::new());
        let accepted = spawn_upstream(listener, release.clone(), None);
        let pool = WsUpstreamPool::new();
        let (mut slow, _) = pool
            .open_turn(
                dial.clone(),
                json!({"type":"response.create","model":"slow"}),
            )
            .await
            .unwrap();
        // The second turn finishes on its own connection while the first waits.
        let fast = tokio::time::timeout(
            Duration::from_secs(2),
            completed_id(
                &pool,
                dial.clone(),
                json!({"type":"response.create","model":"m"}),
            ),
        )
        .await
        .expect("a concurrent turn must not queue behind a running one");
        assert_eq!(fast, "resp_1_1");
        release.notify_one();
        let slow_id = response_id(&slow.recv().await.unwrap().unwrap()).unwrap();
        assert_eq!(slow_id, "resp_0_1");
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn follow_ups_return_to_the_connection_that_answered() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dial = local_dial(listener.local_addr().unwrap());
        let release = Arc::new(Notify::new());
        spawn_upstream(listener, release.clone(), None);
        let pool = WsUpstreamPool::new();
        // Open two connections: a slow turn on the first, a quick one on the second.
        let (mut slow, _) = pool
            .open_turn(
                dial.clone(),
                json!({"type":"response.create","model":"slow"}),
            )
            .await
            .unwrap();
        let second = completed_id(
            &pool,
            dial.clone(),
            json!({"type":"response.create","model":"m"}),
        )
        .await;
        assert_eq!(second, "resp_1_1");
        release.notify_one();
        slow.recv().await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Both connections are idle now; the follow-up still goes to the second.
        let follow_up = completed_id(
            &pool,
            dial,
            json!({"type":"response.create","model":"m","previous_response_id": second}),
        )
        .await;
        assert_eq!(follow_up, "resp_1_2");
    }

    #[tokio::test]
    async fn same_thread_continuation_reuses_socket_when_turn_metadata_changes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let release = Arc::new(Notify::new());
        let accepted = spawn_upstream(listener, release, None);
        let pool = WsUpstreamPool::new();
        let first_dial = scoped_dial(addr, "window-1", "thread-1", "turn-a");
        let first = completed_id(
            &pool,
            first_dial,
            json!({"type":"response.create","model":"m"}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let continuation = completed_id(
            &pool,
            scoped_dial(addr, "window-1", "thread-1", "turn-b"),
            json!({
                "type":"response.create",
                "model":"m",
                "previous_response_id": first
            }),
        )
        .await;
        assert_eq!(continuation, "resp_0_2");
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn previous_response_id_cannot_take_over_another_window() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = spawn_upstream(listener, Arc::new(Notify::new()), None);
        let pool = WsUpstreamPool::new();
        let first = completed_id(
            &pool,
            scoped_dial(addr, "window-1", "thread-1", "turn-a"),
            json!({"type":"response.create","model":"m"}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let second = completed_id(
            &pool,
            scoped_dial(addr, "window-2", "thread-2", "turn-b"),
            json!({
                "type":"response.create",
                "model":"m",
                "previous_response_id": first
            }),
        )
        .await;
        assert_eq!(second, "resp_1_1");
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_windows_keep_separate_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let release = Arc::new(Notify::new());
        let accepted = spawn_upstream(listener, release.clone(), None);
        let pool = WsUpstreamPool::new();
        let (mut slow, _) = pool
            .open_turn(
                scoped_dial(addr, "window-1", "thread-1", "turn-a"),
                json!({"type":"response.create","model":"slow"}),
            )
            .await
            .unwrap();
        let fast = completed_id(
            &pool,
            scoped_dial(addr, "window-2", "thread-2", "turn-b"),
            json!({"type":"response.create","model":"m"}),
        )
        .await;
        assert_eq!(fast, "resp_1_1");
        release.notify_one();
        assert_eq!(
            response_id(&slow.recv().await.unwrap().unwrap()).as_deref(),
            Some("resp_0_1")
        );
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn maintain_preserves_other_window_connections() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = spawn_upstream(listener, Arc::new(Notify::new()), None);
        let pool = WsUpstreamPool::new();
        let first = scoped_dial(addr, "window-1", "thread-1", "turn-a");
        let second = scoped_dial(addr, "window-2", "thread-2", "turn-a");
        pool.maintain(&first).await;
        pool.maintain(&second).await;
        assert_eq!(pool.open_connections(), 2);
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        pool.maintain(&first).await;
        assert_eq!(pool.open_connections(), 2);
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_connection_closed_while_idle_is_redialed() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dial = local_dial(listener.local_addr().unwrap());
        let accepted = spawn_upstream(listener, Arc::new(Notify::new()), Some(0));
        let pool = WsUpstreamPool::new();
        let first = completed_id(
            &pool,
            dial.clone(),
            json!({"type":"response.create","model":"m"}),
        )
        .await;
        assert_eq!(first, "resp_0_1");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let second = tokio::time::timeout(
            Duration::from_secs(2),
            completed_id(&pool, dial, json!({"type":"response.create","model":"m"})),
        )
        .await
        .unwrap();
        assert_eq!(second, "resp_1_1");
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn maintain_warms_pings_and_rewarms_after_invalidate() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dial = local_dial(listener.local_addr().unwrap());
        let accepted = spawn_upstream(listener, Arc::new(Notify::new()), None);
        let pool = WsUpstreamPool::new();
        pool.maintain(&dial).await;
        assert_eq!(pool.open_connections(), 1);
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        // A healthy warm connection is only pinged, not replaced.
        pool.maintain(&dial).await;
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        // The first turn uses the warm connection instead of dialing.
        let id = completed_id(
            &pool,
            dial.clone(),
            json!({"type":"response.create","model":"m"}),
        )
        .await;
        assert_eq!(id, "resp_0_1");
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        pool.invalidate().await;
        assert_eq!(pool.open_connections(), 0);
        tokio::time::timeout(Duration::from_secs(1), pool.changed())
            .await
            .expect("invalidate wakes the keeper");
        pool.maintain(&dial).await;
        assert_eq!(pool.open_connections(), 1);
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_handshake_backs_off() {
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dial = local_dial(closed.local_addr().unwrap());
        drop(closed);
        let pool = WsUpstreamPool::new();
        assert!(pool.available());
        pool.maintain(&dial).await;
        assert!(!pool.available());
        assert_eq!(pool.open_connections(), 0);
        // Changing the line clears the backoff.
        pool.invalidate().await;
        assert!(pool.available());
    }
}
