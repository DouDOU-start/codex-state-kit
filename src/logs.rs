use futures_util::Stream;
use serde::Serialize;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};
use url::Url;

pub const ROUTE_DEFAULT_SYSTEM: &str = "default_system";
pub const ROUTE_EXPLICIT_PROXY: &str = "explicit_proxy";
pub const ROUTE_EMBEDDED_WARP: &str = "embedded_warp";
pub const ROUTE_EMBEDDED_MIHOMO: &str = "embedded_mihomo";
pub const ROUTE_MANUAL_PROXY: &str = "manual_proxy";
static LOG_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const MAX_LOGS: usize = 80;
const MAX_TOKEN_FETCH_LOGS: usize = 48;
const UNSET_MILLIS: u64 = u64::MAX;
const STREAM_AWAITING: u8 = 0;
const STREAM_ACTIVE: u8 = 1;
const STREAM_FINISHING: u8 = 2;
const STREAM_COMPLETED: u8 = 3;
const STREAM_ERROR: u8 = 4;
const STREAM_CANCELLED: u8 = 5;

#[derive(Debug)]
pub struct StreamLifecycle {
    started: Instant,
    response_header_ms: u64,
    state: AtomicU8,
    first_chunk_ms: AtomicU64,
    last_chunk_ms: AtomicU64,
    stream_total_ms: AtomicU64,
    stream_bytes: AtomicU64,
    stream_chunks: AtomicU64,
    max_idle_ms: AtomicU64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamLifecycleSnapshot {
    pub state: &'static str,
    pub first_chunk_ms: Option<u128>,
    pub last_chunk_ms: Option<u128>,
    pub stream_total_ms: Option<u128>,
    pub stream_bytes: u64,
    pub stream_chunks: u64,
    pub max_idle_ms: Option<u128>,
    pub current_idle_ms: Option<u128>,
}

impl StreamLifecycle {
    pub fn new(started: Instant, response_header_ms: u128) -> Self {
        let response_header_ms = millis_to_u64(response_header_ms);
        Self {
            started,
            response_header_ms,
            state: AtomicU8::new(STREAM_AWAITING),
            first_chunk_ms: AtomicU64::new(UNSET_MILLIS),
            last_chunk_ms: AtomicU64::new(UNSET_MILLIS),
            stream_total_ms: AtomicU64::new(UNSET_MILLIS),
            stream_bytes: AtomicU64::new(0),
            stream_chunks: AtomicU64::new(0),
            max_idle_ms: AtomicU64::new(0),
        }
    }

    pub fn observe_chunk(&self, bytes: usize) {
        loop {
            match self.state.load(Ordering::Acquire) {
                STREAM_AWAITING => {
                    if self
                        .state
                        .compare_exchange(
                            STREAM_AWAITING,
                            STREAM_ACTIVE,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_ok()
                    {
                        break;
                    }
                }
                STREAM_ACTIVE => break,
                _ => return,
            }
        }

        let elapsed_ms = elapsed_millis(self.started.elapsed());
        self.stream_bytes
            .fetch_add(u64::try_from(bytes).unwrap_or(u64::MAX), Ordering::Relaxed);
        self.stream_chunks.fetch_add(1, Ordering::Relaxed);
        let previous_ms = self.last_chunk_ms.swap(elapsed_ms, Ordering::Relaxed);
        let idle_start_ms = if previous_ms == UNSET_MILLIS {
            self.response_header_ms
        } else {
            previous_ms
        };
        self.max_idle_ms
            .fetch_max(elapsed_ms.saturating_sub(idle_start_ms), Ordering::Relaxed);
        let _ = self.first_chunk_ms.compare_exchange(
            UNSET_MILLIS,
            elapsed_ms,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }

    pub fn complete(&self) {
        self.finish(STREAM_COMPLETED);
    }

    pub fn error(&self) {
        self.finish(STREAM_ERROR);
    }

    pub fn cancel(&self) {
        self.finish(STREAM_CANCELLED);
    }

    pub fn snapshot(&self) -> StreamLifecycleSnapshot {
        let state = self.state.load(Ordering::Acquire);
        let elapsed_ms = elapsed_millis(self.started.elapsed());
        let chunks = self.stream_chunks.load(Ordering::Relaxed);
        let last_chunk_ms = self.last_chunk_ms.load(Ordering::Relaxed);
        StreamLifecycleSnapshot {
            state: match state {
                STREAM_AWAITING => "awaiting_first_chunk",
                STREAM_ACTIVE | STREAM_FINISHING => "streaming",
                STREAM_COMPLETED => "completed",
                STREAM_ERROR => "error",
                STREAM_CANCELLED => "cancelled",
                _ => "unknown",
            },
            first_chunk_ms: optional_millis(self.first_chunk_ms.load(Ordering::Relaxed)),
            last_chunk_ms: optional_millis(last_chunk_ms),
            stream_total_ms: optional_millis(self.stream_total_ms.load(Ordering::Relaxed)),
            stream_bytes: self.stream_bytes.load(Ordering::Relaxed),
            stream_chunks: chunks,
            max_idle_ms: (chunks > 0
                || matches!(state, STREAM_COMPLETED | STREAM_ERROR | STREAM_CANCELLED))
            .then(|| u128::from(self.max_idle_ms.load(Ordering::Relaxed))),
            current_idle_ms: matches!(state, STREAM_AWAITING | STREAM_ACTIVE | STREAM_FINISHING)
                .then(|| {
                    let last_activity_ms = if last_chunk_ms == UNSET_MILLIS {
                        self.response_header_ms
                    } else {
                        last_chunk_ms
                    };
                    u128::from(elapsed_ms.saturating_sub(last_activity_ms))
                }),
        }
    }

    fn finish(&self, final_state: u8) {
        loop {
            let state = self.state.load(Ordering::Acquire);
            if !matches!(state, STREAM_AWAITING | STREAM_ACTIVE) {
                return;
            }
            if self
                .state
                .compare_exchange(state, STREAM_FINISHING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }
        let elapsed_ms = elapsed_millis(self.started.elapsed());
        let last_chunk_ms = self.last_chunk_ms.load(Ordering::Relaxed);
        let last_activity_ms = if last_chunk_ms == UNSET_MILLIS {
            self.response_header_ms
        } else {
            last_chunk_ms
        };
        self.max_idle_ms.fetch_max(
            elapsed_ms.saturating_sub(last_activity_ms),
            Ordering::Relaxed,
        );
        self.stream_total_ms.store(elapsed_ms, Ordering::Relaxed);
        self.state.store(final_state, Ordering::Release);
    }
}

pub struct ObservedStream<S> {
    inner: Pin<Box<S>>,
    lifecycle: Arc<StreamLifecycle>,
    finished: bool,
}

impl<S> ObservedStream<S> {
    pub fn new(stream: S, lifecycle: Arc<StreamLifecycle>) -> Self {
        Self {
            inner: Box::pin(stream),
            lifecycle,
            finished: false,
        }
    }
}

impl<S, B, E> Stream for ObservedStream<S>
where
    S: Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    type Item = Result<B, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.lifecycle.observe_chunk(chunk.as_ref().len());
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                this.finished = true;
                this.lifecycle.error();
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finished = true;
                this.lifecycle.complete();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S> Drop for ObservedStream<S> {
    fn drop(&mut self) {
        if !self.finished {
            self.lifecycle.cancel();
        }
    }
}

fn elapsed_millis(duration: Duration) -> u64 {
    millis_to_u64(duration.as_millis())
}

fn millis_to_u64(value: u128) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX - 1)
}

fn optional_millis(value: u64) -> Option<u128> {
    (value != UNSET_MILLIS).then(|| u128::from(value))
}

#[derive(Clone, Debug, Default)]
pub struct NetworkLogDetails {
    pub state_policy: Option<crate::settings::StateMissPolicy>,
    pub account_id: Option<String>,
    pub account_email: Option<String>,
    pub flow: String,
    pub transport: String,
    pub target_origin: String,
    pub final_origin: Option<String>,
    pub route_kind: String,
    pub proxy_endpoint: Option<String>,
    pub proxy_session: Option<String>,
    pub peer_addr: Option<String>,
    pub http_version: Option<String>,
    pub model: Option<String>,
    pub upstream_response_model: Option<String>,
    pub content_encoding: String,
    pub body_bytes: usize,
    pub turn_state_action: String,
    pub turn_state_len: Option<usize>,
    pub returned_turn_state_len: Option<usize>,
    pub error_kind: Option<String>,
    pub response_status: Option<u16>,
    pub response_header_ms: Option<u128>,
    pub response_content_encoding: Option<String>,
    pub first_token_ms: Option<u128>,
    pub output_tokens: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub in_progress: bool,
    pub stream_lifecycle: Option<Arc<StreamLifecycle>>,
    pub diag: Option<crate::diag::Request>,
    pub token_fp: Option<String>,
    pub cookie_names: Vec<String>,
    /// 本次请求实际注入的票据。只用于响应观测，不进入日志。
    pub injected_token: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub state_policy: Option<crate::settings::StateMissPolicy>,
    pub account_id: Option<String>,
    pub account_email: Option<String>,
    pub id: u64,
    pub ts: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    /// Kept for compatibility with older renderers. This is response-header latency.
    pub ms: u128,
    pub response_header_ms: Option<u128>,
    pub response_content_encoding: Option<String>,
    pub first_token_ms: Option<u128>,
    pub output_tokens: Option<u64>,
    pub tokens_per_second: Option<f64>,
    pub in_progress: bool,
    pub flow: String,
    pub transport: String,
    pub target_origin: String,
    pub final_origin: Option<String>,
    pub route_kind: String,
    pub proxy_endpoint: Option<String>,
    pub proxy_session: Option<String>,
    pub peer_addr: Option<String>,
    pub http_version: Option<String>,
    pub model: Option<String>,
    pub upstream_response_model: Option<String>,
    pub content_encoding: String,
    pub body_bytes: usize,
    pub turn_state_action: String,
    pub turn_state_len: Option<usize>,
    pub returned_turn_state_len: Option<usize>,
    pub error_kind: Option<String>,
    pub stream_state: String,
    pub first_chunk_ms: Option<u128>,
    pub last_chunk_ms: Option<u128>,
    pub stream_total_ms: Option<u128>,
    pub stream_bytes: u64,
    pub stream_chunks: u64,
    pub max_idle_ms: Option<u128>,
    pub current_idle_ms: Option<u128>,
    #[serde(skip)]
    stream_lifecycle: Option<Arc<StreamLifecycle>>,
}

impl LogEntry {
    pub fn new(
        method: &str,
        path: &str,
        status: u16,
        started: Instant,
        details: NetworkLogDetails,
    ) -> Self {
        let ms = details
            .response_header_ms
            .unwrap_or_else(|| started.elapsed().as_millis());
        let stream_lifecycle = details.stream_lifecycle.clone();
        let stream = stream_lifecycle
            .as_ref()
            .map(|lifecycle| lifecycle.snapshot());
        Self {
            state_policy: details.state_policy,
            id: LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed),
            account_id: details.account_id,
            account_email: details.account_email,
            ts: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
            method: method.to_string(),
            path: path.to_string(),
            status,
            ms,
            response_header_ms: details.response_header_ms,
            response_content_encoding: details.response_content_encoding,
            first_token_ms: details.first_token_ms,
            output_tokens: details.output_tokens,
            tokens_per_second: details.tokens_per_second,
            in_progress: details.in_progress,
            flow: details.flow,
            transport: details.transport,
            target_origin: details.target_origin,
            final_origin: details.final_origin,
            route_kind: details.route_kind,
            proxy_endpoint: details.proxy_endpoint,
            proxy_session: details.proxy_session,
            peer_addr: details.peer_addr,
            http_version: details.http_version,
            model: details.model,
            upstream_response_model: details.upstream_response_model,
            content_encoding: details.content_encoding,
            body_bytes: details.body_bytes,
            turn_state_action: details.turn_state_action,
            turn_state_len: details.turn_state_len,
            returned_turn_state_len: details.returned_turn_state_len,
            error_kind: details.error_kind,
            stream_state: stream
                .as_ref()
                .map(|snapshot| snapshot.state)
                .unwrap_or("not_tracked")
                .into(),
            first_chunk_ms: stream.as_ref().and_then(|snapshot| snapshot.first_chunk_ms),
            last_chunk_ms: stream.as_ref().and_then(|snapshot| snapshot.last_chunk_ms),
            stream_total_ms: stream
                .as_ref()
                .and_then(|snapshot| snapshot.stream_total_ms),
            stream_bytes: stream
                .as_ref()
                .map(|snapshot| snapshot.stream_bytes)
                .unwrap_or(0),
            stream_chunks: stream
                .as_ref()
                .map(|snapshot| snapshot.stream_chunks)
                .unwrap_or(0),
            max_idle_ms: stream.as_ref().and_then(|snapshot| snapshot.max_idle_ms),
            current_idle_ms: stream
                .as_ref()
                .and_then(|snapshot| snapshot.current_idle_ms),
            stream_lifecycle,
        }
    }

    pub fn snapshot(&self) -> Self {
        let mut entry = self.clone();
        let Some(lifecycle) = &self.stream_lifecycle else {
            return entry;
        };
        let stream = lifecycle.snapshot();
        entry.stream_state = stream.state.into();
        entry.first_chunk_ms = stream.first_chunk_ms;
        entry.last_chunk_ms = stream.last_chunk_ms;
        entry.stream_total_ms = stream.stream_total_ms;
        entry.stream_bytes = stream.stream_bytes;
        entry.stream_chunks = stream.stream_chunks;
        entry.max_idle_ms = stream.max_idle_ms;
        entry.current_idle_ms = stream.current_idle_ms;
        entry
    }
}

pub fn safe_text(raw: &str, max_chars: usize) -> String {
    raw.trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(max_chars)
        .collect()
}

pub fn endpoint_origin(raw: &str) -> String {
    let Ok(url) = Url::parse(raw.trim()) else {
        return "invalid-endpoint".into();
    };
    let Some(host) = url.host_str() else {
        return "invalid-endpoint".into();
    };
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    match url.port_or_known_default() {
        Some(port) => format!("{}://{}:{}", url.scheme(), host, port),
        None => format!("{}://{}", url.scheme(), host),
    }
}

pub fn network_details(upstream: &str, proxy: &str) -> NetworkLogDetails {
    let proxy = proxy.trim();
    NetworkLogDetails {
        flow: "business".into(),
        transport: "http".into(),
        target_origin: endpoint_origin(upstream),
        route_kind: if proxy.is_empty() {
            ROUTE_DEFAULT_SYSTEM.into()
        } else {
            ROUTE_EXPLICIT_PROXY.into()
        },
        proxy_endpoint: (!proxy.is_empty()).then(|| endpoint_origin(proxy)),
        content_encoding: "none".into(),
        turn_state_action: "not_applicable".into(),
        ..NetworkLogDetails::default()
    }
}

pub fn token_network_details(
    upstream: &str,
    proxy: &str,
    embedded_warp: bool,
    model: &str,
) -> NetworkLogDetails {
    NetworkLogDetails {
        flow: "token_fetch".into(),
        transport: "http_sse".into(),
        target_origin: endpoint_origin(upstream),
        route_kind: if embedded_warp {
            ROUTE_EMBEDDED_WARP.into()
        } else {
            ROUTE_MANUAL_PROXY.into()
        },
        proxy_endpoint: Some(endpoint_origin(proxy)),
        model: Some(safe_text(model, 80)).filter(|value| !value.is_empty()),
        content_encoding: "json".into(),
        turn_state_action: "awaiting_response".into(),
        ..NetworkLogDetails::default()
    }
}

pub fn safe_content_encoding(raw: Option<&str>) -> String {
    let value = raw.unwrap_or("none").trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "none" | "identity" => "none".into(),
        "gzip" | "x-gzip" | "br" | "deflate" | "zstd" => value,
        _ => "other".into(),
    }
}

pub fn request_error_kind(error: &reqwest::Error) -> String {
    if error.is_connect() {
        "connect"
    } else if error.is_timeout() {
        "timeout"
    } else if error.is_request() {
        "request"
    } else if error.is_body() {
        "body"
    } else if error.is_decode() {
        "decode"
    } else {
        "upstream"
    }
    .into()
}

/// Tracks response-level timing without retaining or logging response content.
/// For SSE, first-token timing follows the visible-output event (`delta`, text,
/// tool arguments, or image content) rather than response headers/preamble.
#[derive(Clone, Debug, Default)]
pub struct ResponseMetrics {
    first_token_ms: Option<u128>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    usage_seen: bool,
    upstream_response_model: Option<String>,
    line: Vec<u8>,
    data: Vec<u8>,
    skip_event: bool,
    after_cr: bool,
    is_sse: bool,
    completed: bool,
    pub error_kind: Option<&'static str>,
    sse_events: Vec<(String, u32)>,
}

// Decode only the statistics side channel. The proxy forwards original bytes.
// The writer feeds the bounded event parser directly, never collecting a whole
// decompressed response in memory.
#[derive(Default)]
struct MetricsSink {
    metrics: ResponseMetrics,
    elapsed_ms: u128,
    is_sse: bool,
}

impl std::io::Write for MetricsSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.metrics.observe(bytes, self.elapsed_ms, self.is_sse);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

enum MetricsDecoder {
    Plain(MetricsSink),
    Zstd(zstd::stream::write::Decoder<'static, MetricsSink>),
    Gzip(flate2::write::GzDecoder<MetricsSink>),
    Deflate(flate2::write::ZlibDecoder<MetricsSink>),
}

pub struct ResponseBodyMetrics {
    decoder: MetricsDecoder,
    disabled: bool,
}

impl ResponseBodyMetrics {
    pub fn new(encoding: &str) -> Self {
        let decoder = match encoding.trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Some(MetricsDecoder::Plain(MetricsSink::default())),
            "zstd" => zstd::stream::write::Decoder::new(MetricsSink::default())
                .and_then(|mut decoder| {
                    decoder.window_log_max(23)?;
                    Ok(MetricsDecoder::Zstd(decoder))
                })
                .ok(),
            "gzip" | "x-gzip" => Some(MetricsDecoder::Gzip(flate2::write::GzDecoder::new(
                MetricsSink::default(),
            ))),
            "deflate" => Some(MetricsDecoder::Deflate(flate2::write::ZlibDecoder::new(
                MetricsSink::default(),
            ))),
            _ => None,
        };
        let disabled = decoder.is_none();
        Self {
            decoder: decoder.unwrap_or_else(|| MetricsDecoder::Plain(MetricsSink::default())),
            disabled,
        }
    }

    fn sink_mut(&mut self) -> &mut MetricsSink {
        match &mut self.decoder {
            MetricsDecoder::Plain(sink) => sink,
            MetricsDecoder::Zstd(decoder) => decoder.get_mut(),
            MetricsDecoder::Gzip(decoder) => decoder.get_mut(),
            MetricsDecoder::Deflate(decoder) => decoder.get_mut(),
        }
    }

    pub fn observe(&mut self, bytes: &[u8], elapsed_ms: u128, is_sse: bool) {
        use std::io::Write;
        if self.disabled {
            return;
        }
        let sink = self.sink_mut();
        sink.elapsed_ms = elapsed_ms;
        sink.is_sse = is_sse;
        let writer: &mut dyn Write = match &mut self.decoder {
            MetricsDecoder::Plain(sink) => sink,
            MetricsDecoder::Zstd(decoder) => decoder,
            MetricsDecoder::Gzip(decoder) => decoder,
            MetricsDecoder::Deflate(decoder) => decoder,
        };
        if writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .is_err()
        {
            // A statistics decoding failure must not interrupt forwarding or
            // turn a successful HTTP request into a network error.
            self.disabled = true;
            self.sink_mut().metrics = ResponseMetrics::default();
        }
    }

    pub fn finish(&mut self, elapsed_ms: u128) {
        if !self.disabled {
            self.sink_mut().metrics.finish(elapsed_ms);
        }
    }
}

impl std::ops::Deref for ResponseBodyMetrics {
    type Target = ResponseMetrics;

    fn deref(&self) -> &Self::Target {
        let sink = match &self.decoder {
            MetricsDecoder::Plain(sink) => sink,
            MetricsDecoder::Zstd(decoder) => decoder.get_ref(),
            MetricsDecoder::Gzip(decoder) => decoder.get_ref(),
            MetricsDecoder::Deflate(decoder) => decoder.get_ref(),
        };
        &sink.metrics
    }
}

impl ResponseMetrics {
    pub fn observe(&mut self, chunk: &[u8], elapsed_ms: u128, is_sse: bool) {
        self.is_sse = is_sse;
        if !is_sse {
            if !self.skip_event && self.data.len() + chunk.len() <= 128 * 1024 {
                self.data.extend_from_slice(chunk);
            } else {
                self.data.clear();
                self.skip_event = true;
            }
            return;
        }
        // Handle LF, CRLF, CR and arbitrary network/UTF-8 chunk boundaries. Bound
        // inspection memory per event, and recover after oversized events.
        for &byte in chunk {
            if byte == b'\n' && self.after_cr {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\n' || byte == b'\r' {
                if self.line.is_empty() {
                    if !self.skip_event {
                        let data = std::mem::take(&mut self.data);
                        self.observe_event(&data, elapsed_ms);
                    }
                    self.data.clear();
                    self.skip_event = false;
                } else {
                    if let Some(value) = self.line.strip_prefix(b"data:") {
                        let value = value.strip_prefix(b" ").unwrap_or(value);
                        if self.data.len() + value.len() + 1 <= 128 * 1024 {
                            self.data.extend_from_slice(value);
                            self.data.push(b'\n');
                        } else {
                            self.skip_event = true;
                        }
                    }
                    self.line.clear();
                }
            } else if self.line.len() < 128 * 1024 {
                self.line.push(byte);
            } else {
                self.skip_event = true;
            }
        }
    }

    pub fn finish(&mut self, elapsed_ms: u128) {
        if self.is_sse {
            self.observe(b"\n\n", elapsed_ms, true);
        } else if !self.skip_event {
            let data = std::mem::take(&mut self.data);
            self.observe_event(&data, elapsed_ms);
        }
    }

    pub fn first_token_ms(&self) -> Option<u128> {
        self.first_token_ms
    }

    pub fn output_tokens(&self) -> Option<u64> {
        self.output_tokens
    }

    /// Number of input/prompt tokens reported by the provider.
    ///
    /// Both Responses API (`input_tokens`) and Chat Completions
    /// (`prompt_tokens`) usage shapes are supported.
    pub fn input_tokens(&self) -> Option<u64> {
        self.input_tokens
    }

    /// Number of input tokens served from the provider's prompt cache.
    ///
    /// This is read from the nested `*_tokens_details.cached_tokens` fields
    /// used by OpenAI, along with the common top-level cache aliases.
    pub fn cached_input_tokens(&self) -> Option<u64> {
        self.cached_input_tokens
    }

    /// Returns true once a provider usage object was observed, even when the
    /// object omits one or more token counters (or reports zero).
    pub fn usage_seen(&self) -> bool {
        self.usage_seen
    }

    pub fn upstream_response_model(&self) -> Option<&str> {
        self.upstream_response_model.as_deref()
    }

    pub fn completed(&self) -> bool {
        self.completed
    }

    pub fn sse_event_summary(&self) -> Option<String> {
        if self.sse_events.is_empty() {
            return None;
        }
        Some(
            self.sse_events
                .iter()
                .map(|(name, count)| {
                    if *count == 1 {
                        name.clone()
                    } else {
                        format!("{name}×{count}")
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    fn record_sse_event_type(&mut self, event_type: &str) {
        let name = safe_text(event_type, 80);
        if name.is_empty() {
            return;
        }
        if self
            .sse_events
            .last()
            .is_some_and(|(last, _)| last == &name)
        {
            if let Some((_, count)) = self.sse_events.last_mut() {
                *count = count.saturating_add(1);
            }
            return;
        }
        if self.sse_events.len() < 40 {
            self.sse_events.push((name, 1));
        }
    }

    fn observe_event(&mut self, event: &[u8], elapsed_ms: u128) {
        let Ok(json) = serde_json::from_slice::<serde_json::Value>(event) else {
            return;
        };
        if let Some(event_type) = json.get("type").and_then(serde_json::Value::as_str) {
            self.record_sse_event_type(event_type);
        }
        let terminal = matches!(
            json.get("type").and_then(serde_json::Value::as_str),
            Some(
                "response.completed"
                    | "response.done"
                    | "response.failed"
                    | "response.incomplete"
                    | "response.cancelled"
                    | "response.canceled"
            )
        );
        // Read only provider metadata, never model-shaped fields in generated
        // text, output items or tool arguments. Keep the first declaration;
        // a terminal event overrides it, as in sub2api's model observer.
        if self.upstream_response_model.is_none() || terminal {
            if let Some(model) = [json.pointer("/response/model"), json.get("model")]
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .map(|model| safe_text(model, 80))
                .find(|model| !model.is_empty())
            {
                self.upstream_response_model = Some(model);
            }
        }
        if self.is_sse
            && json.get("type").and_then(serde_json::Value::as_str) == Some("response.completed")
        {
            self.completed = true;
        }
        self.error_kind = match json.get("type").and_then(serde_json::Value::as_str) {
            Some("response.failed" | "error") => Some("response_failed"),
            Some("response.incomplete") => Some("response_incomplete"),
            _ => self.error_kind,
        };
        if self.is_sse && self.first_token_ms.is_none() && has_visible_output(&json) {
            self.first_token_ms = Some(elapsed_ms);
        }
        // Usage metadata is emitted in either the top-level `usage` object
        // (Chat Completions and non-streaming Responses) or nested under
        // `response.usage` (Responses SSE terminal events). Parse both when
        // present so a provider can split counters across the two envelopes.
        if let Some(usage) = json.get("usage").filter(|usage| usage.is_object()) {
            self.observe_usage(usage);
        }
        if let Some(usage) = json
            .pointer("/response/usage")
            .filter(|usage| usage.is_object())
        {
            self.observe_usage(usage);
        }
    }

    fn observe_usage(&mut self, usage: &serde_json::Value) {
        self.usage_seen = true;
        if let Some(tokens) = find_usage_tokens(usage, &["input_tokens", "prompt_tokens"]) {
            self.input_tokens = Some(tokens);
        }
        if let Some(tokens) = find_usage_tokens(usage, &["output_tokens", "completion_tokens"]) {
            self.output_tokens = Some(tokens);
        }
        if let Some(tokens) = find_cached_input_tokens(usage) {
            self.cached_input_tokens = Some(tokens);
        }
    }
}

fn has_visible_output(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let event_type = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let visible_event = matches!(
        event_type,
        "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.reasoning_text.delta"
            | "response.audio_transcript.delta"
            | "response.refusal.delta"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta"
            | "response.image_generation_call.partial_image"
    );
    if visible_event {
        for key in ["delta", "partial_image_b64"] {
            if object
                .get(key)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.is_empty())
            {
                return true;
            }
        }
    }
    let field = match event_type {
        "response.output_text.done"
        | "response.reasoning_summary_text.done"
        | "response.reasoning_text.done"
        | "response.audio_transcript.done" => Some("text"),
        "response.function_call_arguments.done" => Some("arguments"),
        "response.custom_tool_call_input.done" => Some("input"),
        _ => None,
    };
    if field.is_some_and(|key| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.is_empty())
    }) {
        return true;
    }
    if matches!(
        event_type,
        "response.content_part.added"
            | "response.content_part.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
    ) {
        return ["/part/text", "/part/transcript"].iter().any(|path| {
            value
                .pointer(path)
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.is_empty())
        });
    }
    false
}

fn find_usage_tokens(usage: &serde_json::Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| usage.get(*key).and_then(serde_json::Value::as_u64))
}

fn find_cached_input_tokens(usage: &serde_json::Value) -> Option<u64> {
    // OpenAI uses `input_tokens_details` for Responses and
    // `prompt_tokens_details` for Chat Completions. A few compatible
    // providers use the singular spelling, so accept it as well. Keep the
    // lookup constrained to the usage object to avoid charging token-shaped
    // fields in generated output/tool arguments.
    for details_key in [
        "input_tokens_details",
        "prompt_tokens_details",
        "input_token_details",
        "prompt_token_details",
    ] {
        let Some(details) = usage.get(details_key).filter(|value| value.is_object()) else {
            continue;
        };
        if let Some(tokens) = find_usage_tokens(
            details,
            &[
                "cached_tokens",
                "cached_input_tokens",
                "cache_read_input_tokens",
                "cache_read_tokens",
            ],
        ) {
            return Some(tokens);
        }
    }

    find_usage_tokens(
        usage,
        &[
            "cached_tokens",
            "cached_input_tokens",
            "cache_read_input_tokens",
            "cache_read_tokens",
        ],
    )
}

pub fn push(logs: &mut VecDeque<LogEntry>, entry: LogEntry) {
    if entry.flow == "token_fetch" {
        let token_count = logs
            .iter()
            .filter(|existing| existing.flow == "token_fetch")
            .count();
        if token_count >= MAX_TOKEN_FETCH_LOGS {
            if let Some(index) = logs
                .iter()
                .position(|existing| existing.flow == "token_fetch")
            {
                logs.remove(index);
            }
        } else if logs.len() >= MAX_LOGS {
            logs.pop_front();
        }
    } else if logs.len() >= MAX_LOGS {
        logs.pop_front();
    }
    logs.push_back(entry);
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use futures_util::StreamExt;

    #[test]
    fn response_model_prefers_terminal_metadata_and_ignores_output_content() {
        let mut metrics = ResponseMetrics::default();
        for event in [
            r#"{"type":"response.output_text.delta","delta":"{\"model\":\"fake\"}"}"#,
            r#"{"type":"response.output_item.done","item":{"model":"fake"}}"#,
            r#"{"output":[{"model":"fake"}],"data":[{"model":"fake"}]}"#,
            r#"{"response":{"model":123},"model":null}"#,
            r#"{"response":{"model":"broken"}"#,
        ] {
            metrics.observe(format!("data: {event}\n\n").as_bytes(), 10, true);
            assert_eq!(metrics.upstream_response_model(), None);
        }
        for (event, expected) in [
            (
                r#"{"type":"response.created","response":{"model":"gpt-5.6-sol"}}"#,
                "gpt-5.6-sol",
            ),
            (
                r#"{"type":"response.in_progress","response":{"model":"ignored"}}"#,
                "gpt-5.6-sol",
            ),
            (
                r#"{"type":"response.completed","response":{"model":"gpt-6-sol"},"model":"ignored"}"#,
                "gpt-6-sol",
            ),
            (
                r#"{"type":"response.created","response":{"model":"ignored"}}"#,
                "gpt-6-sol",
            ),
            (
                r#"{"type":"response.completed","response":{"model":" "}}"#,
                "gpt-6-sol",
            ),
        ] {
            for byte in format!("data: {event}\r\n\r\n").bytes() {
                metrics.observe(&[byte], 20, true);
            }
            assert_eq!(metrics.upstream_response_model(), Some(expected));
        }
    }

    #[test]
    fn response_model_reads_json_and_chat_chunks_with_bounded_safe_names() {
        for (body, expected) in [
            (
                r#"{"object":"response","model":" gpt-6-sol\n "}"#.to_owned(),
                "gpt-6-sol".to_owned(),
            ),
            (
                r#"{"response":{"model":""},"model":"gpt-6-astra"}"#.to_owned(),
                "gpt-6-astra".to_owned(),
            ),
            (
                serde_json::json!({"model": "模".repeat(200)}).to_string(),
                "模".repeat(80),
            ),
        ] {
            let mut metrics = ResponseMetrics::default();
            metrics.observe(body.as_bytes(), 10, false);
            metrics.finish(20);
            assert_eq!(metrics.upstream_response_model(), Some(expected.as_str()));
        }
        let mut metrics = ResponseMetrics::default();
        metrics.observe(
            b"data: {\"object\":\"chat.completion.chunk\",\"model\":\"gpt-6-sol\"}\n\n",
            10,
            true,
        );
        assert_eq!(metrics.upstream_response_model(), Some("gpt-6-sol"));
        let mut metrics = ResponseMetrics::default();
        metrics.observe(b"{\"data\":[{\"id\":\"model-list-entry\"}]}", 10, false);
        metrics.finish(20);
        assert_eq!(metrics.upstream_response_model(), None);
    }

    #[test]
    fn compressed_metrics_parse_fragmented_streams() {
        use std::io::Write;
        let events = [
            b"data: {\"type\":\"response.created\",\"response\":{\"model\":\"gpt-5.6-sol\"}}\n\n".as_slice(),
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hello\"}\n\n".as_slice(),
            b"data: {\"type\":\"response.completed\",\"response\":{\"model\":\"gpt-6-sol\",\"usage\":{\"output_tokens\":120}}}\n\n".as_slice(),
        ];
        for encoding in ["zstd", "gzip", "deflate"] {
            let mut chunks = Vec::new();
            macro_rules! encode {
                ($encoder:expr) => {{
                    let mut encoder = $encoder;
                    for event in &events[..2] {
                        encoder.write_all(event).unwrap();
                        encoder.flush().unwrap();
                        chunks.push(std::mem::take(encoder.get_mut()));
                    }
                    encoder.write_all(events[2]).unwrap();
                    chunks.push(encoder.finish().unwrap());
                }};
            }
            match encoding {
                "zstd" => encode!(zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap()),
                "gzip" => encode!(flate2::write::GzEncoder::new(
                    Vec::new(),
                    flate2::Compression::fast()
                )),
                _ => encode!(flate2::write::ZlibEncoder::new(
                    Vec::new(),
                    flate2::Compression::fast()
                )),
            }
            let mut metrics = ResponseBodyMetrics::new(encoding);
            for (index, chunk) in chunks.iter().enumerate() {
                for byte in chunk {
                    metrics.observe(&[*byte], (index as u128 + 1) * 50, true);
                }
                assert_eq!(
                    metrics.first_token_ms(),
                    (index > 0).then_some(100),
                    "{encoding}"
                );
                assert_eq!(
                    metrics.upstream_response_model(),
                    Some(if index == 2 {
                        "gpt-6-sol"
                    } else {
                        "gpt-5.6-sol"
                    }),
                    "{encoding}"
                );
            }
            metrics.finish(200);
            assert_eq!(metrics.output_tokens(), Some(120), "{encoding}");
            assert!(!metrics.disabled, "{encoding}");
        }
    }

    #[test]
    fn unsupported_or_invalid_compression_does_not_invent_metrics() {
        for encoding in ["br", "gzip, zstd", "zstd", "gzip", "deflate"] {
            let mut metrics = ResponseBodyMetrics::new(encoding);
            metrics.observe(
                b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
                25,
                true,
            );
            metrics.finish(50);
            assert!(metrics.disabled, "{encoding}");
            assert_eq!(metrics.first_token_ms(), None);
            assert_eq!(metrics.output_tokens(), None);
            assert_eq!(metrics.upstream_response_model(), None);
            assert_eq!(metrics.error_kind, None);
        }
    }

    #[test]
    fn sse_event_summary_collapses_repeats_and_skips_payloads() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(b"data: {\"type\":\"response.created\"}\n\n", 1, true);
        metrics.observe(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n",
            2,
            true,
        );
        metrics.observe(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"secret-payload\"}\n\n",
            3,
            true,
        );
        metrics.observe(b"data: {\"type\":\"response.completed\"}\n\n", 4, true);
        assert_eq!(
            metrics.sse_event_summary().as_deref(),
            Some("response.created, response.output_text.delta×2, response.completed")
        );
        assert!(!metrics
            .sse_event_summary()
            .unwrap()
            .contains("secret-payload"));
    }

    #[test]
    fn sse_metrics_ignore_preamble_and_parse_split_utf8_crlf_and_usage() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(
            b": keepalive\r\ndata: {\"type\":\"response.created\"}\r\n\r\n",
            10,
            true,
        );
        metrics.observe(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"\"}\n\n",
            20,
            true,
        );
        assert_eq!(metrics.first_token_ms(), None);
        let text = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你好\"}\r\n\r\n";
        for byte in text.as_bytes() {
            metrics.observe(&[*byte], 50, true);
        }
        metrics.observe(b"data: {\"type\":\"response.completed\",\n", 60, true);
        metrics.observe(
            b"data: \"response\":{\"usage\":{\"output_tokens\":120}}}\n\ndata: [DONE]\n\n",
            70,
            true,
        );
        metrics.finish(80);
        assert_eq!(metrics.first_token_ms(), Some(50));
        assert_eq!(metrics.output_tokens(), Some(120));
    }

    #[test]
    fn sse_metrics_track_tools_images_and_never_use_output_as_usage() {
        for event in [
            r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking"}"#,
            r#"{"type":"response.content_part.added","part":{"text":"hi"}}"#,
            r#"{"type":"response.output_text.done","text":"hi"}"#,
            r#"{"type":"response.function_call_arguments.delta","delta":"{}"}"#,
            r#"{"type":"response.custom_tool_call_input.delta","delta":"ls"}"#,
            r#"{"type":"response.image_generation_call.partial_image","partial_image_b64":"abc"}"#,
        ] {
            let mut metrics = ResponseMetrics::default();
            metrics.observe(format!("data: {event}\n\n").as_bytes(), 25, true);
            assert_eq!(metrics.first_token_ms(), Some(25));
        }
        let mut metrics = ResponseMetrics::default();
        metrics.observe(b"data: {\"output\":{\"output_tokens\":999}}\n\n", 30, true);
        assert_eq!(metrics.output_tokens(), None);
        assert!(!metrics.usage_seen());
    }

    #[test]
    fn metrics_recover_from_oversized_events_and_keep_memory_bounded() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(&vec![b'x'; 512 * 1024], 10, true);
        assert!(metrics.line.len() <= 128 * 1024);
        metrics.observe(b"\n\ndata: {\"usage\":{\"output_tokens\":0}}\n\n", 20, true);
        assert_eq!(metrics.output_tokens(), Some(0));
        assert_eq!(metrics.first_token_ms(), None);
    }

    #[test]
    fn metrics_read_json_usage_without_inventing_nonstream_ttft() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(b"{\"usage\":{\"completion_tokens\":42}}", 10, false);
        metrics.finish(20);
        assert_eq!(metrics.output_tokens(), Some(42));
        assert_eq!(metrics.first_token_ms(), None);
    }

    #[test]
    fn metrics_parse_responses_and_chat_completion_usage_shapes() {
        let mut responses = ResponseMetrics::default();
        responses.observe(
            br#"data: {"type":"response.completed","response":{"usage":{"input_tokens":120,"output_tokens":34,"input_tokens_details":{"cached_tokens":17}}}}

"#,
            50,
            true,
        );
        assert!(responses.usage_seen());
        assert_eq!(responses.input_tokens(), Some(120));
        assert_eq!(responses.output_tokens(), Some(34));
        assert_eq!(responses.cached_input_tokens(), Some(17));

        let mut chat = ResponseMetrics::default();
        chat.observe(
            br#"{"usage":{"prompt_tokens":80,"completion_tokens":13,"prompt_tokens_details":{"cached_tokens":9}}}"#,
            10,
            false,
        );
        chat.finish(20);
        assert!(chat.usage_seen());
        assert_eq!(chat.input_tokens(), Some(80));
        assert_eq!(chat.output_tokens(), Some(13));
        assert_eq!(chat.cached_input_tokens(), Some(9));
    }

    #[test]
    fn metrics_usage_seen_distinguishes_empty_usage_from_absent_usage() {
        let mut empty = ResponseMetrics::default();
        empty.observe(br#"{"usage":{}}"#, 10, false);
        empty.finish(20);
        assert!(empty.usage_seen());
        assert_eq!(empty.input_tokens(), None);
        assert_eq!(empty.output_tokens(), None);
        assert_eq!(empty.cached_input_tokens(), None);

        let mut absent = ResponseMetrics::default();
        absent.observe(br#"{"response":{"output":[{"text":"usage"}]}}"#, 10, false);
        absent.finish(20);
        assert!(!absent.usage_seen());
    }

    #[test]
    fn metrics_parse_top_level_cached_input_aliases_without_output_content() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(
            br#"{"usage":{"input_tokens":3,"output_tokens":4,"cached_input_tokens":2,"output":{"cached_tokens":999}}}"#,
            10,
            false,
        );
        metrics.finish(20);
        assert!(metrics.usage_seen());
        assert_eq!(metrics.input_tokens(), Some(3));
        assert_eq!(metrics.output_tokens(), Some(4));
        assert_eq!(metrics.cached_input_tokens(), Some(2));
    }

    #[test]
    fn metrics_capture_sse_error_class_without_error_message() {
        let mut metrics = ResponseMetrics::default();
        metrics.observe(
            b"data: {\"type\":\"response.failed\",\"error\":{\"message\":\"private\"}}\n\n",
            10,
            true,
        );
        assert_eq!(metrics.error_kind, Some("response_failed"));
        assert_eq!(metrics.first_token_ms(), None);
    }

    #[test]
    fn endpoint_origin_keeps_route_and_drops_credentials() {
        assert_eq!(
            endpoint_origin("socks5h://user:secret@127.0.0.1:1080/path?token=hidden"),
            "socks5h://127.0.0.1:1080"
        );
        assert_eq!(
            endpoint_origin("https://chatgpt.com/backend-api/codex"),
            "https://chatgpt.com:443"
        );
    }

    #[test]
    fn network_details_distinguishes_explicit_and_default_routes() {
        let direct = network_details("https://chatgpt.com/backend-api/codex", "");
        assert_eq!(direct.route_kind, ROUTE_DEFAULT_SYSTEM);
        assert!(direct.proxy_endpoint.is_none());

        let proxied = network_details(
            "https://chatgpt.com/backend-api/codex",
            "http://user:secret@localhost:7897",
        );
        assert_eq!(proxied.route_kind, ROUTE_EXPLICIT_PROXY);
        assert_eq!(
            proxied.proxy_endpoint.as_deref(),
            Some("http://localhost:7897")
        );
    }

    #[test]
    fn token_details_identify_warp_without_exposing_credentials() {
        let details = token_network_details(
            "https://chatgpt.com/backend-api/codex",
            "socks5h://statekit:private@127.0.0.1:1080",
            true,
            "gpt-6-astra",
        );
        assert_eq!(details.flow, "token_fetch");
        assert_eq!(details.route_kind, ROUTE_EMBEDDED_WARP);
        assert_eq!(
            details.proxy_endpoint.as_deref(),
            Some("socks5h://127.0.0.1:1080")
        );
        assert_eq!(details.model.as_deref(), Some("gpt-6-astra"));
    }

    #[test]
    fn token_details_identify_manual_proxy() {
        let details = token_network_details(
            "https://chatgpt.com/backend-api/codex",
            "socks5h://proxy.example.test:44445",
            false,
            "gpt-6-astra",
        );
        assert_eq!(details.route_kind, ROUTE_MANUAL_PROXY);
        assert_eq!(
            details.proxy_endpoint.as_deref(),
            Some("socks5h://proxy.example.test:44445")
        );
    }

    #[test]
    fn safe_text_removes_log_controls_and_bounds_input() {
        assert_eq!(safe_text("  gpt-6\nastra\t-extra", 12), "gpt-6astra-e");
    }

    #[test]
    fn token_fetch_bursts_do_not_evict_all_business_logs() {
        let mut entries = VecDeque::new();
        for _ in 0..60 {
            push(
                &mut entries,
                LogEntry::new(
                    "POST",
                    "/responses",
                    200,
                    Instant::now(),
                    network_details("https://chatgpt.com", ""),
                ),
            );
        }
        for _ in 0..100 {
            push(
                &mut entries,
                LogEntry::new(
                    "POST",
                    "/responses",
                    502,
                    Instant::now(),
                    token_network_details(
                        "https://chatgpt.com",
                        "socks5h://127.0.0.1:1080",
                        true,
                        "gpt-6-astra",
                    ),
                ),
            );
        }

        assert_eq!(entries.len(), MAX_LOGS);
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.flow == "token_fetch")
                .count(),
            MAX_TOKEN_FETCH_LOGS
        );
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.flow == "business")
                .count(),
            MAX_LOGS - MAX_TOKEN_FETCH_LOGS
        );
    }

    #[test]
    fn stream_lifecycle_tracks_chunks_idle_time_and_completion() {
        let started = Instant::now() - Duration::from_millis(80);
        let lifecycle = StreamLifecycle::new(started, 20);
        lifecycle.observe_chunk(12);
        lifecycle.observe_chunk(30);
        lifecycle.complete();

        let snapshot = lifecycle.snapshot();
        assert_eq!(snapshot.state, "completed");
        assert_eq!(snapshot.stream_bytes, 42);
        assert_eq!(snapshot.stream_chunks, 2);
        assert!(snapshot.first_chunk_ms.is_some_and(|ms| ms >= 80));
        assert!(snapshot.last_chunk_ms.is_some_and(|ms| ms >= 80));
        assert!(snapshot.stream_total_ms.is_some_and(|ms| ms >= 80));
        assert!(snapshot.max_idle_ms.is_some_and(|ms| ms >= 60));
    }

    #[test]
    fn terminal_stream_state_cannot_be_overwritten_by_drop_cancellation() {
        let lifecycle = StreamLifecycle::new(Instant::now(), 0);
        lifecycle.observe_chunk(4);
        lifecycle.error();
        lifecycle.cancel();
        assert_eq!(lifecycle.snapshot().state, "error");
    }

    #[test]
    fn terminal_stream_state_includes_silence_before_first_chunk() {
        let started = Instant::now() - Duration::from_millis(80);
        let lifecycle = StreamLifecycle::new(started, 20);
        lifecycle.complete();
        let snapshot = lifecycle.snapshot();
        assert_eq!(snapshot.stream_chunks, 0);
        assert!(snapshot.max_idle_ms.is_some_and(|ms| ms >= 60));
    }

    #[test]
    fn terminal_stream_state_includes_trailing_silence() {
        let lifecycle = StreamLifecycle::new(Instant::now(), 0);
        lifecycle.observe_chunk(1);
        std::thread::sleep(Duration::from_millis(20));
        lifecycle.error();
        assert!(lifecycle.snapshot().max_idle_ms.is_some_and(|ms| ms >= 15));

        let cancelled = StreamLifecycle::new(Instant::now(), 0);
        cancelled.observe_chunk(1);
        std::thread::sleep(Duration::from_millis(20));
        cancelled.cancel();
        assert!(cancelled.snapshot().max_idle_ms.is_some_and(|ms| ms >= 15));
    }

    #[test]
    fn log_snapshot_reads_live_stream_metrics_without_logging_content() {
        let started = Instant::now();
        let lifecycle = Arc::new(StreamLifecycle::new(started, 7));
        let details = NetworkLogDetails {
            response_header_ms: Some(7),
            stream_lifecycle: Some(lifecycle.clone()),
            ..network_details("https://chatgpt.com", "")
        };
        let entry = LogEntry::new("POST", "/responses", 200, started, details);
        assert_eq!(entry.snapshot().stream_state, "awaiting_first_chunk");

        lifecycle.observe_chunk(128);
        let active = entry.snapshot();
        assert_eq!(active.stream_state, "streaming");
        assert_eq!(active.stream_bytes, 128);
        assert_eq!(active.stream_chunks, 1);

        lifecycle.complete();
        assert_eq!(entry.snapshot().stream_state, "completed");
    }

    #[tokio::test]
    async fn observed_stream_records_success_error_and_downstream_cancellation() {
        let completed = Arc::new(StreamLifecycle::new(Instant::now(), 0));
        let chunks = stream::iter(vec![Ok::<_, &'static str>(vec![1_u8, 2]), Ok(vec![3])]);
        let output: Vec<_> = ObservedStream::new(chunks, completed.clone())
            .collect()
            .await;
        assert_eq!(output.len(), 2);
        let snapshot = completed.snapshot();
        assert_eq!(snapshot.state, "completed");
        assert_eq!(snapshot.stream_bytes, 3);
        assert_eq!(snapshot.stream_chunks, 2);

        let failed = Arc::new(StreamLifecycle::new(Instant::now(), 0));
        let chunks = stream::iter(vec![Ok::<_, &'static str>(vec![1_u8]), Err("broken")]);
        let output: Vec<_> = ObservedStream::new(chunks, failed.clone()).collect().await;
        assert!(output[1].is_err());
        assert_eq!(failed.snapshot().state, "error");

        let cancelled = Arc::new(StreamLifecycle::new(Instant::now(), 0));
        let stream = ObservedStream::new(
            stream::pending::<Result<Vec<u8>, &'static str>>(),
            cancelled.clone(),
        );
        drop(stream);
        assert_eq!(cancelled.snapshot().state, "cancelled");
    }
}
