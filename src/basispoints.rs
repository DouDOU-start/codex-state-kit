//! Compatibility translation for the ChatGPT Excel/Basispoints Responses API.
//!
//! Basispoints rejects caller supplied `tools`.  Client tools are therefore
//! described in a developer message and transported through its native
//! `run_officejs` function.  The proxy never executes Office code.

use anyhow::{anyhow, Result};
use axum::body::Bytes;
use futures_util::Stream;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

pub const DEFAULT_ENDPOINT: &str = "https://bps.openai.com/basispoints/api/responses";
pub const TRANSPORT_NAME: &str = "run_officejs";
const MAX_LINEAGES: usize = 128;
const MAX_CALLS_PER_LINEAGE: usize = 512;

#[derive(Clone, Debug)]
struct ToolSpec {
    name: String,
    kind: String,
    namespace: Option<String>,
    schema: Value,
}

#[derive(Clone, Debug, Default)]
struct NativeCall {
    item: Value,
}

#[derive(Clone, Debug, Default)]
struct Lineage {
    tools: HashMap<String, ToolSpec>,
    calls: HashMap<String, NativeCall>,
    pending: HashMap<String, Value>,
}

#[derive(Clone, Debug, Default)]
pub struct BpsState {
    lineages: HashMap<String, Lineage>,
}

#[derive(Clone, Debug)]
pub struct PreparedRequest {
    pub body: Vec<u8>,
    pub lineage: String,
}

fn digest(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn string_field(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn lineage_key(body: &Value, input: &Value) -> String {
    for key in [
        "prompt_cache_key",
        "promptCacheKey",
        "session_id",
        "sessionId",
    ] {
        if let Some(value) = string_field(body.get(key)) {
            return value;
        }
    }
    for key in ["thread_id", "session_id"] {
        if let Some(value) = body.pointer(&format!("/client_metadata/{key}")) {
            if let Some(value) = string_field(Some(value)) {
                return value;
            }
        }
    }
    if let Some(items) = input.as_array() {
        if let Some(first) = items.first() {
            return format!("root:{}", digest(first));
        }
    }
    "anonymous".into()
}

fn turn_fingerprint(input: &Value) -> String {
    let Some(items) = input.as_array() else {
        return digest(input);
    };
    let last_user = items
        .iter()
        .rposition(|item| item.get("role").and_then(Value::as_str) == Some("user"));
    let end = last_user.map(|i| i + 1).unwrap_or(1).min(items.len());
    digest(&Value::Array(items[..end].to_vec()))
}

fn tool_key(namespace: Option<&str>, name: &str) -> String {
    namespace
        .map(|ns| format!("{ns}.{name}"))
        .unwrap_or_else(|| name.to_owned())
}

fn collect_tools(
    raw: Option<&Value>,
    namespace: Option<&str>,
    out: &mut HashMap<String, ToolSpec>,
) {
    let Some(tools) = raw.and_then(Value::as_array) else {
        return;
    };
    for tool in tools {
        let Some(kind) = tool.get("type").and_then(Value::as_str) else {
            continue;
        };
        if kind == "namespace" {
            let next = tool
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty());
            collect_tools(tool.get("tools"), next.or(namespace), out);
            continue;
        }
        let Some(name) = tool
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            continue;
        };
        let key = tool_key(namespace, name);
        let schema = tool
            .get("parameters")
            .or_else(|| tool.get("input_schema"))
            .or_else(|| tool.get("inputSchema"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        out.insert(
            key,
            ToolSpec {
                name: name.to_owned(),
                kind: kind.to_owned(),
                namespace: namespace.map(str::to_owned),
                schema,
            },
        );
    }
}

fn developer_catalog(tools: &HashMap<String, ToolSpec>) -> String {
    let mut entries = Vec::new();
    let mut keys: Vec<_> = tools.keys().collect();
    keys.sort();
    for key in keys {
        let tool = &tools[key];
        entries.push(json!({"name": key, "tool": tool.name, "namespace": tool.namespace, "type": tool.kind, "parameters": tool.schema}));
    }
    format!(
        "This request is relayed through the Basispoints Responses API. Client tools are available through the native {TRANSPORT_NAME} transport; it never executes OfficeJS. Call {TRANSPORT_NAME} exactly once per client tool request. Its code field is JSON text containing one object with name and arguments (or input for custom tools). Do not put JavaScript or another transport envelope in code. Available client tools: {}",
        serde_json::to_string(&entries).unwrap_or_else(|_| "[]".into())
    )
}

fn message(text: String) -> Value {
    json!({"type":"message","role":"developer","content":[{"type":"input_text","text":text}]})
}

fn metadata_string_map(raw: Option<&Value>) -> Map<String, Value> {
    let mut result = Map::new();
    if let Some(object) = raw.and_then(Value::as_object) {
        for (key, value) in object {
            if let Some(text) = value.as_str() {
                result.insert(
                    key.chars().take(64).collect(),
                    json!(text.chars().take(512).collect::<String>()),
                );
            } else if value.is_number() || value.is_boolean() {
                result.insert(key.chars().take(64).collect(), json!(value.to_string()));
            }
        }
    }
    result
}

fn basispoints_model(raw: Option<&Value>) -> String {
    let model = raw
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if model.contains("luna") {
        "gpt-5.6-luna".into()
    } else if model.contains("terra") {
        "gpt-5.6-terra".into()
    } else {
        "gpt-5.6-sol".into()
    }
}

fn basispoints_effort(object: &Map<String, Value>) -> String {
    let raw = object
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| {
            object
                .get("reasoning")
                .and_then(|value| value.get("effort"))
                .and_then(Value::as_str)
        })
        .unwrap_or("medium")
        .to_ascii_lowercase();
    match raw.as_str() {
        "low" => "low".into(),
        "high" => "high".into(),
        "xhigh" | "x-high" | "extra-high" | "extra_high" | "max" => "xhigh".into(),
        _ => "medium".into(),
    }
}

pub async fn prepare_request(state: &Arc<Mutex<BpsState>>, raw: &[u8]) -> Result<PreparedRequest> {
    let plain = decode_body(raw)?;
    let mut body: Value =
        serde_json::from_slice(&plain).map_err(|_| anyhow!("BPS 请求体不是 JSON"))?;
    let client_turn = string_field(body.pointer("/client_metadata/turn_id"));
    let input = body.get("input").cloned().unwrap_or_else(|| json!([]));
    let lineage = lineage_key(&body, &input);
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow!("BPS 请求体必须是 JSON 对象"))?;
    let effort = basispoints_effort(object);
    let mut tools = HashMap::new();
    collect_tools(object.get("tools"), None, &mut tools);
    let mut guard = state.lock().await;
    if guard.lineages.len() > MAX_LINEAGES {
        if let Some(oldest) = guard.lineages.keys().next().cloned() {
            if oldest != lineage {
                guard.lineages.remove(&oldest);
            }
        }
    }
    let entry = guard.lineages.entry(lineage.clone()).or_default();
    if !tools.is_empty() {
        entry.tools = tools.clone();
    }
    let tools = entry.tools.clone();
    let fingerprint = turn_fingerprint(&input);
    let turn_id = string_field(object.get("metadata").and_then(|m| m.get("turn_id")))
        .or(client_turn)
        .unwrap_or_else(|| format!("turn_{}", &digest(&json!([lineage, fingerprint]))[..24]));
    let iteration = input
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| {
                    matches!(
                        item.get("type").and_then(Value::as_str),
                        Some("function_call_output" | "custom_tool_call_output")
                    )
                })
                .count()
                + 1
        })
        .unwrap_or(1);

    let mut translated_input = input.as_array().cloned().unwrap_or_default();
    for item in &mut translated_input {
        let Some(kind) = item.get("type").and_then(Value::as_str) else {
            continue;
        };
        if matches!(kind, "function_call" | "custom_tool_call") {
            if let Some(call_id) = string_field(item.get("call_id")) {
                if let Some(native) = entry.calls.get(&call_id).map(|call| call.item.clone()) {
                    *item = native;
                }
            }
        }
    }
    object.insert(
        "input".into(),
        Value::Array(std::mem::take(&mut translated_input)),
    );
    object.remove("tools");
    object.remove("tool_choice");
    let mut metadata = metadata_string_map(object.get("metadata"));
    metadata.insert("turn_id".into(), json!(turn_id));
    metadata.insert("agent_iteration".into(), json!(iteration.to_string()));
    metadata.insert(
        "task_id".into(),
        json!(format!("bps_{}", &digest(&json!(lineage))[..24])),
    );
    object.insert("metadata".into(), Value::Object(metadata));
    let model = basispoints_model(object.get("model"));
    object.insert("model".into(), json!(model));
    object.insert("model_selection".into(), json!("explicit"));
    object.insert("reasoning_effort".into(), json!(effort));
    object.insert("store".into(), json!(false));
    if !object
        .get("context_management")
        .is_some_and(Value::is_array)
    {
        object.insert(
            "context_management".into(),
            json!([{"type":"compaction","compact_threshold":200000}]),
        );
    }
    for key in [
        "instructions",
        "reasoning",
        "client_metadata",
        "parallel_tool_calls",
        "stream_options",
        "include",
        "service_tier",
        "text",
    ] {
        object.remove(key);
    }
    let mut prefix = vec![message(developer_catalog(&tools))];
    if let Some(instructions) = object
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
    {
        prefix.insert(0, message(instructions.to_owned()));
    }
    let current_input = object.remove("input").unwrap_or_else(|| json!([]));
    let mut all = prefix;
    match current_input {
        Value::Array(mut items) => all.append(&mut items),
        Value::String(text) => all.push(
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]}),
        ),
        other => all.push(other),
    }
    object.insert("input".into(), Value::Array(all));
    Ok(PreparedRequest {
        body: serde_json::to_vec(&body)?,
        lineage,
    })
}

fn decode_body(raw: &[u8]) -> Result<Vec<u8>> {
    if raw.len() >= 4 && raw[..4] == [0x28, 0xB5, 0x2F, 0xFD] {
        return zstd::decode_all(std::io::Cursor::new(raw))
            .map_err(|_| anyhow!("无法解压 zstd 请求体"));
    }
    if raw.len() >= 2 && raw[..2] == [0x1F, 0x8B] {
        let mut decoder = flate2::read::GzDecoder::new(raw);
        let mut plain = Vec::new();
        std::io::Read::read_to_end(&mut decoder, &mut plain)
            .map_err(|_| anyhow!("无法解压 gzip 请求体"))?;
        return Ok(plain);
    }
    if serde_json::from_slice::<Value>(raw).is_ok() {
        return Ok(raw.to_vec());
    }
    let mut decoder = flate2::read::DeflateDecoder::new(raw);
    let mut plain = Vec::new();
    std::io::Read::read_to_end(&mut decoder, &mut plain)
        .map_err(|_| anyhow!("无法解压 deflate 请求体"))?;
    Ok(plain)
}

fn transport_arguments(value: &Value) -> Option<(String, Value)> {
    let args = value.get("arguments").and_then(Value::as_str).or_else(|| {
        value
            .get("arguments")
            .and_then(Value::as_object)
            .map(|_| "")
    })?;
    let outer: Value = if args.is_empty() {
        value.get("arguments")?.clone()
    } else {
        serde_json::from_str(args).ok()?
    };
    let code = outer.get("code").and_then(Value::as_str)?;
    let inner: Value = serde_json::from_str(code).ok()?;
    let name = inner
        .get("name")
        .or_else(|| inner.get("tool"))
        .and_then(Value::as_str)?
        .to_owned();
    let payload = inner
        .get("arguments")
        .or_else(|| inner.get("args"))
        .or_else(|| inner.get("input"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    Some((name, payload))
}

fn translated_item(native: &Value, spec: &ToolSpec, payload: &Value) -> Value {
    let call_id = native
        .get("call_id")
        .cloned()
        .unwrap_or_else(|| json!("call_unknown"));
    let id = native
        .get("id")
        .cloned()
        .unwrap_or_else(|| json!(format!("fc_{}", call_id.as_str().unwrap_or("unknown"))));
    let args = serde_json::to_string(payload).unwrap_or_else(|_| "{}".into());
    if spec.kind == "custom" {
        json!({"type":"custom_tool_call","id":id,"call_id":call_id,"name":spec.name,"input":args,"status":"completed"})
    } else {
        json!({"type":"function_call","id":id,"call_id":call_id,"name":spec.name,"arguments":args,"status":"completed"})
    }
}

pub async fn transform_event(
    state: &Arc<Mutex<BpsState>>,
    lineage: &str,
    event: &Value,
) -> Option<Vec<Value>> {
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if matches!(
        kind,
        "response.completed" | "response.failed" | "response.incomplete" | "error"
    ) {
        return Some(vec![event.clone()]);
    }
    let item = event
        .get("item")
        .or_else(|| event.pointer("/response/output/0"));
    let is_transport = item.and_then(|v| v.get("name")).and_then(Value::as_str)
        == Some(TRANSPORT_NAME)
        || event.get("name").and_then(Value::as_str) == Some(TRANSPORT_NAME);
    if !is_transport && !kind.contains("function_call_arguments") {
        return Some(vec![event.clone()]);
    }
    if kind == "response.output_item.added" {
        if let Some(item) =
            item.filter(|v| v.get("name").and_then(Value::as_str) == Some(TRANSPORT_NAME))
        {
            let key = string_field(item.get("id"))
                .or_else(|| string_field(item.get("call_id")))
                .unwrap_or_else(|| "pending".into());
            state
                .lock()
                .await
                .lineages
                .entry(lineage.to_owned())
                .or_default()
                .pending
                .insert(key, item.clone());
            return Some(Vec::new());
        }
    }
    if kind == "response.function_call_arguments.done" || kind == "response.output_item.done" {
        let key = string_field(event.get("item_id"))
            .or_else(|| item.and_then(|v| string_field(v.get("id"))));
        let mut native = {
            let mut guard = state.lock().await;
            let lineage_state = guard.lineages.entry(lineage.to_owned()).or_default();
            let key = key.or_else(|| item.and_then(|v| string_field(v.get("call_id"))));
            key.and_then(|key| lineage_state.pending.remove(&key))
                .or_else(|| {
                    item.filter(|v| v.get("name").and_then(Value::as_str) == Some(TRANSPORT_NAME))
                        .cloned()
                })
        }?;
        if let Some(arguments) = event.get("arguments").and_then(Value::as_str) {
            native["arguments"] = json!(arguments);
        }
        let Some((name, payload)) = transport_arguments(&native) else {
            return Some(Vec::new());
        };
        let (spec, call_id) = {
            let guard = state.lock().await;
            let lineage_state = guard.lineages.get(lineage)?;
            (
                lineage_state.tools.get(&name)?.clone(),
                string_field(native.get("call_id")).unwrap_or_else(|| "call_unknown".into()),
            )
        };
        let translated = translated_item(&native, &spec, &payload);
        state.lock().await.lineages.get_mut(lineage).map(|l| {
            l.calls.insert(call_id, NativeCall { item: native });
            while l.calls.len() > MAX_CALLS_PER_LINEAGE {
                if let Some(key) = l.calls.keys().next().cloned() {
                    l.calls.remove(&key);
                } else {
                    break;
                }
            }
        });
        let mut output =
            vec![json!({"type":"response.output_item.added","item":translated.clone()})];
        if translated.get("type").and_then(Value::as_str) == Some("function_call") {
            output.push(json!({
                "type": "response.function_call_arguments.done",
                "item_id": translated.get("id").cloned().unwrap_or(Value::Null),
                "call_id": translated.get("call_id").cloned().unwrap_or(Value::Null),
                "arguments": translated.get("arguments").cloned().unwrap_or_else(|| json!("{}")),
            }));
        }
        output.push(json!({"type":"response.output_item.done","item":translated}));
        return Some(output);
    }
    Some(Vec::new())
}

pub fn sse_stream<S>(
    stream: S,
    state: Arc<Mutex<BpsState>>,
    lineage: String,
) -> impl Stream<Item = Result<Bytes, axum::Error>>
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Unpin + Send + 'static,
{
    futures_util::stream::unfold(
        (stream, Vec::<u8>::new()),
        move |(mut stream, mut buffer)| {
            let state = state.clone();
            let lineage = lineage.clone();
            async move {
                loop {
                    let boundary = buffer
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|pos| (pos, 4))
                        .or_else(|| {
                            buffer
                                .windows(2)
                                .position(|w| w == b"\n\n")
                                .map(|pos| (pos, 2))
                        });
                    if let Some((pos, separator_len)) = boundary {
                        let block: Vec<u8> = buffer.drain(..pos + separator_len).collect();
                        let text = String::from_utf8_lossy(&block);
                        let data = text
                            .lines()
                            .find_map(|line| line.strip_prefix("data:").map(str::trim));
                        if let Some(data) = data {
                            if let Ok(event) = serde_json::from_str::<Value>(data) {
                                let transformed = transform_event(&state, &lineage, &event)
                                    .await
                                    .unwrap_or_default();
                                let mut out = Vec::new();
                                for value in transformed {
                                    out.extend_from_slice(
                                        format!(
                                            "data: {}\n\n",
                                            serde_json::to_string(&value)
                                                .unwrap_or_else(|_| "{}".into())
                                        )
                                        .as_bytes(),
                                    );
                                }
                                if !out.is_empty() {
                                    return Some((Ok(Bytes::from(out)), (stream, buffer)));
                                }
                                continue;
                            }
                        }
                        return Some((Ok(Bytes::from(block)), (stream, buffer)));
                    }
                    match stream.next().await {
                        Some(Ok(bytes)) => buffer.extend_from_slice(&bytes),
                        Some(Err(err)) => {
                            return Some((
                                Err(axum::Error::new(std::io::Error::other(err.to_string()))),
                                (stream, buffer),
                            ))
                        }
                        None => {
                            if buffer.is_empty() {
                                return None;
                            }
                            let out = std::mem::take(&mut buffer);
                            return Some((Ok(Bytes::from(out)), (stream, buffer)));
                        }
                    }
                }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn strips_tools_and_injects_catalog() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({"model":"gpt-5.6-sol","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}],"tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}],"tool_choice":"auto"});
        let prepared = prepare_request(&state, serde_json::to_vec(&request).unwrap().as_slice())
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("get_weather"));
        assert_eq!(body["metadata"]["agent_iteration"], "1");
    }

    #[tokio::test]
    async fn translates_nested_run_officejs_call_and_keeps_native_item() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({"input":[{"role":"user","content":[{"type":"input_text","text":"weather"}]}],"tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}]});
        let prepared = prepare_request(&state, serde_json::to_vec(&request).unwrap().as_slice())
            .await
            .unwrap();
        let native = json!({"type":"function_call","id":"fc_native","call_id":"call_native","name":"run_officejs","arguments":serde_json::to_string(&json!({"summary":"weather","code":serde_json::to_string(&json!({"name":"get_weather","arguments":{"city":"Tokyo"}})).unwrap()})).unwrap()});
        let added = transform_event(
            &state,
            &prepared.lineage,
            &json!({"type":"response.output_item.added","item":native.clone()}),
        )
        .await
        .unwrap();
        assert!(added.is_empty());
        let done = transform_event(
            &state,
            &prepared.lineage,
            &json!({"type":"response.output_item.done","item":native}),
        )
        .await
        .unwrap();
        assert_eq!(done[0]["item"]["type"], "function_call");
        assert_eq!(done[0]["item"]["name"], "get_weather");
        assert_eq!(done[0]["item"]["arguments"], "{\"city\":\"Tokyo\"}");
        let replay = json!({"input":[{"role":"user","content":[{"type":"input_text","text":"weather"}]},{"type":"function_call","id":"fc_native","call_id":"call_native","name":"get_weather","arguments":"{\"city\":\"Tokyo\"}"},{"type":"function_call_output","call_id":"call_native","output":"18C"}],"metadata":{"turn_id":"fixed"}});
        let replay = prepare_request(&state, serde_json::to_vec(&replay).unwrap().as_slice())
            .await
            .unwrap();
        let replay: Value = serde_json::from_slice(&replay.body).unwrap();
        assert_eq!(replay["metadata"]["agent_iteration"], "2");
        assert_eq!(replay["input"][2]["name"], "run_officejs");
    }

    #[tokio::test]
    async fn maps_max_effort_to_xhigh() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({"reasoning":{"effort":"max"},"input":"hello"});
        let prepared = prepare_request(&state, serde_json::to_vec(&request).unwrap().as_slice())
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert!(body.get("reasoning").is_none());
    }

    #[tokio::test]
    async fn emits_basispoints_wire_shape() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({
            "model":"gpt-6-astra",
            "instructions":"hi",
            "client_metadata":{"thread_id":"t"},
            "reasoning":{"effort":"high"},
            "input":"hello",
            "stream":true
        });
        let prepared = prepare_request(&state, serde_json::to_vec(&request).unwrap().as_slice())
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["model"], "gpt-5.6-sol");
        assert_eq!(body["model_selection"], "explicit");
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("reasoning").is_none());
        assert!(body.get("instructions").is_none());
        assert!(body.get("client_metadata").is_none());
        assert_eq!(body["context_management"][0]["type"], "compaction");
    }
}
