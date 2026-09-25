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
    description: Value,
    format: Value,
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
    /// Restores the exact native transport item after a reconnect where the
    /// client sends only function_call_output.
    calls: HashMap<String, NativeCall>,
}

#[derive(Clone, Debug)]
pub struct PreparedRequest {
    pub body: Vec<u8>,
    pub lineage: String,
}

/// Returns image content parts from a Responses request in input order.
pub fn image_parts(raw: &[u8]) -> Result<Vec<Value>> {
    let plain = decode_body(raw)?;
    let body: Value = serde_json::from_slice(&plain).map_err(|_| anyhow!("请求体不是 JSON"))?;
    let mut result = Vec::new();
    if let Some(items) = body.get("input").and_then(Value::as_array) {
        for item in items {
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                for part in parts {
                    if matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("input_image" | "image")
                    ) {
                        result.push(part.clone());
                    }
                }
            }
        }
    }
    Ok(result)
}

/// Replaces image content parts with text descriptions before BPS translation.
pub fn replace_image_descriptions(body: &mut Value, descriptions: &[String]) {
    let mut index = 0;
    if let Some(items) = body.get_mut("input").and_then(Value::as_array_mut) {
        for item in items {
            if let Some(parts) = item.get_mut("content").and_then(Value::as_array_mut) {
                for part in parts {
                    if matches!(
                        part.get("type").and_then(Value::as_str),
                        Some("input_image" | "image")
                    ) {
                        let text = descriptions
                            .get(index)
                            .cloned()
                            .unwrap_or_else(|| "[Image description unavailable]".into());
                        *part = json!({"type":"input_text","text":text});
                        index += 1;
                    }
                }
            }
        }
    }
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

fn lineage_key(body: &Value, input: &Value, account_id: Option<&str>) -> String {
    let model = string_field(body.get("model")).unwrap_or_else(|| "unknown-model".into());
    let account = account_id.unwrap_or("unknown-account");
    let scope = |value: String| format!("account:{account}|model:{model}|{value}");
    for key in [
        "prompt_cache_key",
        "promptCacheKey",
        "session_id",
        "sessionId",
    ] {
        if let Some(value) = string_field(body.get(key)) {
            return scope(value);
        }
    }
    for key in ["thread_id", "session_id"] {
        if let Some(value) = body.pointer(&format!("/client_metadata/{key}")) {
            if let Some(value) = string_field(Some(value)) {
                return scope(value);
            }
        }
    }
    if let Some(items) = input.as_array() {
        if let Some(first) = items.first() {
            return scope(format!("root:{}", digest(first)));
        }
    }
    scope("anonymous".into())
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
                description: tool.get("description").cloned().unwrap_or(Value::Null),
                format: tool.get("format").cloned().unwrap_or(Value::Null),
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
        entries.push(json!({"name": key, "tool": tool.name, "namespace": tool.namespace, "type": tool.kind, "parameters": tool.schema, "description": tool.description, "format": tool.format}));
    }
    format!(
        "This request is relayed through the Basispoints Responses API. Client tools are available through the native {TRANSPORT_NAME} transport; it never executes OfficeJS. Call {TRANSPORT_NAME} exactly once per client tool request. Its code field is JSON text containing one object with tool and args (args is an object for function tools and raw text for custom tools). Do not put JavaScript or another transport envelope in code. Available client tools: {}",
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
        "ultra" => "ultra".into(),
        _ => "medium".into(),
    }
}

fn filter_tools_for_choice(tools: &mut HashMap<String, ToolSpec>, choice: Option<&Value>) {
    let Some(choice) = choice else { return };
    let Some(object) = choice.as_object() else {
        return;
    };
    match object.get("type").and_then(Value::as_str) {
        Some("allowed_tools") => {
            let allowed: std::collections::HashSet<String> = object
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|tool| {
                    let name = tool.get("name").and_then(Value::as_str)?;
                    let namespace = tool.get("namespace").and_then(Value::as_str);
                    Some(
                        namespace
                            .map(|ns| format!("{ns}.{name}"))
                            .unwrap_or_else(|| name.to_owned()),
                    )
                })
                .collect();
            tools.retain(|key, spec| allowed.contains(key) || allowed.contains(&spec.name));
        }
        Some("function") | Some("custom") => {
            let name = object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            tools.retain(|key, spec| key == name || spec.name == name);
        }
        _ => {}
    }
}

fn fallback_transport_call(item: &Value) -> Value {
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let call_id = string_field(item.get("call_id")).unwrap_or_else(|| "call_unknown".into());
    let mut inner = Map::new();
    let tool_name = item
        .get("namespace")
        .and_then(Value::as_str)
        .map(|namespace| format!("{namespace}.{name}"))
        .unwrap_or_else(|| name.to_owned());
    inner.insert("tool".into(), json!(tool_name));
    if item.get("type").and_then(Value::as_str) == Some("custom_tool_call") {
        inner.insert(
            "args".into(),
            item.get("input").cloned().unwrap_or_else(|| json!("")),
        );
    } else {
        let arguments = item
            .get("arguments")
            .and_then(Value::as_str)
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
            .unwrap_or_else(|| json!({}));
        inner.insert("args".into(), arguments);
    }
    let outer = json!({
        "summary": format!("Run client tool {name}"),
        "extended_summary": format!("Relay {name} through the external client"),
        "code": serde_json::to_string(&Value::Object(inner)).unwrap_or_else(|_| "{}".into()),
        "destructive": false,
        "references": [],
    });
    json!({
        "type":"function_call",
        "id":format!("fc_{call_id}"),
        "call_id":call_id,
        "name":TRANSPORT_NAME,
        "arguments":serde_json::to_string(&outer).unwrap_or_else(|_| "{}".into()),
        "status":"completed"
    })
}

fn normalize_message_item(item: &Value) -> Option<Value> {
    let role = item
        .get("role")
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "user".into());
    if !matches!(role.as_str(), "developer" | "system" | "user" | "assistant") {
        return None;
    }
    let role = if role == "system" { "developer" } else { &role };
    let text_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    let mut content = Vec::new();
    match item.get("content") {
        Some(Value::String(text)) => content.push(json!({"type":text_type,"text":text})),
        Some(Value::Array(parts)) => {
            for part in parts {
                let Some(kind) = part.get("type").and_then(Value::as_str) else {
                    continue;
                };
                match kind {
                    "input_text" | "output_text" => {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            content.push(json!({"type":text_type,"text":text}));
                        }
                    }
                    "refusal" if role == "assistant" => {
                        if let Some(text) = part
                            .get("refusal")
                            .or_else(|| part.get("text"))
                            .and_then(Value::as_str)
                        {
                            content.push(json!({"type":"refusal","refusal":text}));
                        }
                    }
                    "input_image" | "image" => {
                        // Basispoints' Excel endpoint rejects Responses image
                        // blocks (especially data URLs) with a generic 422.
                        // Keep the turn usable and make the loss explicit to
                        // the model instead of forwarding an invalid item.
                        let hint = part
                            .get("image_url")
                            .and_then(Value::as_str)
                            .filter(|url| url.starts_with("http://") || url.starts_with("https://"))
                            .map(|url| format!(" URL: {url}"))
                            .unwrap_or_default();
                        content.push(json!({
                            "type":text_type,
                            "text":format!("[Image input is unavailable on the Basispoints Excel upstream.{hint}]")
                        }));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    if content.is_empty() {
        return None;
    }
    Some(json!({"type":"message","role":role,"content":content}))
}

fn normalize_input_item(
    item: &Value,
    calls: &HashMap<String, NativeCall>,
    tools: &HashMap<String, ToolSpec>,
) -> Option<Value> {
    let mut item = item.clone();
    if let Some(object) = item.as_object_mut() {
        object.remove("internal_chat_message_metadata_passthrough");
    }
    let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "message" => normalize_message_item(&item),
        "reasoning" => {
            let encrypted = item.get("encrypted_content").and_then(Value::as_str)?;
            if encrypted.is_empty() {
                return None;
            }
            let mut kept = Map::new();
            kept.insert("type".into(), json!("reasoning"));
            // Basispoints accepts encrypted reasoning continuity, but not the
            // richer Codex summary item vocabulary.
            kept.insert("summary".into(), json!([]));
            kept.insert("encrypted_content".into(), json!(encrypted));
            Some(Value::Object(kept))
        }
        "item_reference" => None,
        "function_call" | "custom_tool_call" => {
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            if let Some(call_id) = string_field(item.get("call_id")) {
                if let Some(native) = calls.get(&call_id) {
                    return Some(native.item.clone());
                }
            }
            if name == "update_plan" || name == TRANSPORT_NAME {
                return Some(item);
            }
            (tools.contains_key(name) || item.get("call_id").is_some())
                .then(|| fallback_transport_call(&item))
        }
        "custom_tool_call_output" => {
            let call_id = string_field(item.get("call_id"));
            let mut output = item;
            if let Some(object) = output.as_object_mut() {
                object.insert("type".into(), json!("function_call_output"));
                if let Some(call_id) = call_id.as_deref() {
                    let _ = calls.get(call_id);
                    object.insert("id".into(), json!(format!("fc_{call_id}")));
                }
            }
            Some(output)
        }
        "function_call_output" => {
            let mut output = item;
            if let Some(object) = output.as_object_mut() {
                if let Some(call_id) = string_field(object.get("call_id")) {
                    let _ = calls.get(&call_id);
                    object.insert("id".into(), json!(format!("fc_{call_id}")));
                }
            }
            Some(output)
        }
        // Codex can replay provider-internal records (web search, shell,
        // computer use, MCP discovery, compaction, image generation, etc.).
        // The Excel endpoint does not accept these input item variants. Their
        // user-visible result is already represented by adjacent messages, so
        // dropping the internal record preserves the conversation and avoids
        // a generic 400/422 for an unsupported item type.
        "compaction"
        | "web_search_call"
        | "local_shell_call"
        | "local_shell_call_output"
        | "computer_call"
        | "computer_call_output"
        | "mcp_call"
        | "mcp_list_tools"
        | "mcp_approval_request"
        | "image_generation_call"
        | "code_interpreter_call" => None,
        _ => None,
    }
}

pub async fn prepare_request(
    state: &Arc<Mutex<BpsState>>,
    raw: &[u8],
    account_id: Option<&str>,
) -> Result<PreparedRequest> {
    let plain = decode_body(raw)?;
    let mut body: Value =
        serde_json::from_slice(&plain).map_err(|_| anyhow!("BPS 请求体不是 JSON"))?;
    let client_turn = string_field(body.pointer("/client_metadata/turn_id"));
    let input = body.get("input").cloned().unwrap_or_else(|| json!([]));
    let lineage = lineage_key(&body, &input, account_id);
    let object = body
        .as_object_mut()
        .ok_or_else(|| anyhow!("BPS 请求体必须是 JSON 对象"))?;
    let effort = basispoints_effort(object);
    let instructions = object
        .get("instructions")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned);
    let mut tools = HashMap::new();
    if object
        .get("tool_choice")
        .and_then(Value::as_str)
        .map(|value| value != "none")
        .unwrap_or(true)
    {
        collect_tools(object.get("tools"), None, &mut tools);
        filter_tools_for_choice(&mut tools, object.get("tool_choice"));
    }
    let mut guard = state.lock().await;
    if guard.lineages.len() > MAX_LINEAGES {
        if let Some(oldest) = guard.lineages.keys().next().cloned() {
            if oldest != lineage {
                guard.lineages.remove(&oldest);
            }
        }
    }
    let global_calls = guard.calls.clone();
    let entry = guard.lineages.entry(lineage.clone()).or_default();
    if object.contains_key("tools") {
        entry.tools = tools.clone();
    }
    let tools = entry.tools.clone();
    let mut calls = entry.calls.clone();
    calls.extend(global_calls);
    let fingerprint = turn_fingerprint(&input);
    let turn_id = string_field(object.get("metadata").and_then(|m| m.get("turn_id")))
        .or(client_turn)
        .unwrap_or_else(|| format!("turn_{}", &digest(&json!([lineage, fingerprint]))[..24]));
    let iteration = input
        .as_array()
        .map(|items| {
            let last_user = items
                .iter()
                .rposition(|item| item.get("role").and_then(Value::as_str) == Some("user"));
            let start = last_user.map(|index| index + 1).unwrap_or(0);
            items[start..]
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

    let translated_input = input
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| normalize_input_item(item, &calls, &entry.tools))
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| input.as_str().map(|text| vec![json!({"type":"message","role":"user","content":[{"type":"input_text","text":text}]})]).unwrap_or_default());
    // A follow-up can arrive after the in-memory lineage was evicted or after
    // a client reconnect. BPS rejects an output whose call is not present in
    // the same request, so remove orphaned tool results instead of forwarding
    // an unrecoverable 400.
    let mut seen_calls = std::collections::HashSet::new();
    let mut repaired_input = Vec::with_capacity(translated_input.len());
    for item in translated_input {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or_default();
        if matches!(kind, "function_call" | "custom_tool_call") {
            if let Some(call_id) = string_field(item.get("call_id")) {
                seen_calls.insert(call_id);
            }
            repaired_input.push(item);
            continue;
        }
        if kind == "function_call_output" {
            let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
                continue;
            };
            if !seen_calls.contains(call_id) {
                if let Some(native) = calls.get(call_id) {
                    repaired_input.push(native.item.clone());
                    seen_calls.insert(call_id.to_owned());
                } else {
                    continue;
                }
            }
            repaired_input.push(item);
            continue;
        }
        repaired_input.push(item);
    }
    let translated_input = repaired_input;
    object.insert("input".into(), Value::Array(translated_input));
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
    if let Some(instructions) = instructions {
        prefix.insert(0, message(instructions));
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
    object.insert("stream".into(), json!(true));
    object.retain(|key, _| {
        matches!(
            key.as_str(),
            "model"
                | "model_selection"
                | "reasoning_effort"
                | "store"
                | "stream"
                | "input"
                | "context_management"
                | "metadata"
                | "prompt_cache_key"
        )
    });
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
        json!({"type":"custom_tool_call","id":id,"call_id":call_id,"name":spec.name,"input":payload.as_str().unwrap_or_default(),"status":"completed"})
    } else {
        let mut item = json!({"type":"function_call","id":id,"call_id":call_id,"name":spec.name,"arguments":args,"status":"completed"});
        if let Some(namespace) = &spec.namespace {
            item["namespace"] = json!(namespace);
        }
        item
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
        let mut terminal = event.clone();
        if let Some(items) = terminal
            .pointer_mut("/response/output")
            .and_then(Value::as_array_mut)
        {
            let guard = state.lock().await;
            if let Some(l) = guard.lineages.get(lineage) {
                for item in items {
                    if let Some((name, payload)) = transport_arguments(item) {
                        if let Some(spec) = l.tools.get(&name) {
                            *item = translated_item(item, spec, &payload);
                        }
                    }
                }
            }
        }
        return Some(vec![terminal]);
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
    if kind.contains("function_call_arguments") {
        let guard = state.lock().await;
        let pending = event
            .get("item_id")
            .and_then(Value::as_str)
            .is_some_and(|id| {
                guard
                    .lineages
                    .get(lineage)
                    .is_some_and(|l| l.pending.contains_key(id))
            });
        return Some(if pending { vec![] } else { vec![event.clone()] });
    }
    if kind == "response.output_item.done" {
        let key = string_field(event.get("item_id"))
            .or_else(|| item.and_then(|v| string_field(v.get("id"))));
        let mut native = {
            let mut guard = state.lock().await;
            let lineage_state = guard.lineages.entry(lineage.to_owned()).or_default();
            let key = key.or_else(|| item.and_then(|v| string_field(v.get("call_id"))));
            let pending = key.and_then(|key| lineage_state.pending.remove(&key));
            item.filter(|v| v.get("name").and_then(Value::as_str) == Some(TRANSPORT_NAME))
                .cloned()
                .or(pending)
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
                lineage_state
                    .tools
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| ToolSpec {
                        name: name.clone(),
                        kind: "function".into(),
                        namespace: None,
                        schema: json!({}),
                        description: Value::Null,
                        format: Value::Null,
                    }),
                string_field(native.get("call_id")).unwrap_or_else(|| "call_unknown".into()),
            )
        };
        let translated = translated_item(&native, &spec, &payload);
        let mut state_guard = state.lock().await;
        if let Some(l) = state_guard.lineages.get_mut(lineage) {
            l.calls.insert(call_id, NativeCall { item: native });
            while l.calls.len() > MAX_CALLS_PER_LINEAGE {
                if let Some(key) = l.calls.keys().next().cloned() {
                    l.calls.remove(&key);
                } else {
                    break;
                }
            }
        }
        if let Some(call_id) = translated.get("call_id").and_then(Value::as_str) {
            let native = state_guard
                .lineages
                .get(lineage)
                .and_then(|l| l.calls.get(call_id))
                .cloned();
            if let Some(native) = native {
                state_guard.calls.insert(call_id.to_owned(), native);
            }
        }
        while state_guard.calls.len() > MAX_LINEAGES * MAX_CALLS_PER_LINEAGE {
            if let Some(key) = state_guard.calls.keys().next().cloned() {
                state_guard.calls.remove(&key);
            } else {
                break;
            }
        }
        drop(state_guard);
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
        let prepared = prepare_request(
            &state,
            serde_json::to_vec(&request).unwrap().as_slice(),
            None,
        )
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
    async fn fallback_transport_uses_plugin_tool_args_protocol() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({
            "input":[{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"Tokyo\"}"}],
            "tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}]
        });
        let prepared = prepare_request(&state, &serde_json::to_vec(&request).unwrap(), None)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        let call = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call")
            .unwrap();
        let outer: Value = serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        let inner: Value = serde_json::from_str(outer["code"].as_str().unwrap()).unwrap();
        assert_eq!(inner["tool"], "get_weather");
        assert_eq!(inner["args"]["city"], "Tokyo");
    }

    #[tokio::test]
    async fn translates_nested_run_officejs_call_and_keeps_native_item() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({"input":[{"role":"user","content":[{"type":"input_text","text":"weather"}]}],"tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}]});
        let prepared = prepare_request(
            &state,
            serde_json::to_vec(&request).unwrap().as_slice(),
            None,
        )
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
        let replay = prepare_request(
            &state,
            serde_json::to_vec(&replay).unwrap().as_slice(),
            None,
        )
        .await
        .unwrap();
        let replay: Value = serde_json::from_slice(&replay.body).unwrap();
        assert_eq!(replay["metadata"]["agent_iteration"], "2");
        assert!(replay["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call" && item["name"] == "run_officejs"));
    }

    #[tokio::test]
    async fn maps_max_effort_to_xhigh() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({"reasoning":{"effort":"max"},"input":"hello"});
        let prepared = prepare_request(
            &state,
            serde_json::to_vec(&request).unwrap().as_slice(),
            None,
        )
        .await
        .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["reasoning_effort"], "xhigh");
        assert!(body.get("reasoning").is_none());
    }

    #[tokio::test]
    async fn preserves_ultra_effort_and_filters_allowed_tools() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({
            "reasoning":{"effort":"ultra"},
            "tool_choice":{"type":"allowed_tools","mode":"required","tools":[{"type":"function","name":"get_weather"}]},
            "tools":[
                {"type":"function","name":"get_weather","parameters":{"type":"object"}},
                {"type":"function","name":"write_file","parameters":{"type":"object"}}
            ],
            "input":"hello"
        });
        let prepared = prepare_request(&state, &serde_json::to_vec(&request).unwrap(), None)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["reasoning_effort"], "ultra");
        let catalog = body["input"][0]["content"][0]["text"].as_str().unwrap();
        assert!(catalog.contains("get_weather"));
        assert!(!catalog.contains("write_file"));
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
        let prepared = prepare_request(
            &state,
            serde_json::to_vec(&request).unwrap().as_slice(),
            None,
        )
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

    #[tokio::test]
    async fn filters_codex_history_items_for_bps_replay() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({
            "input":[
                {"type":"reasoning","summary":[]},
                {"type":"item_reference","id":"old"},
                {"role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"custom_tool_call_output","call_id":"call_x","output":"ok"}
            ]
        });
        let prepared = prepare_request(
            &state,
            serde_json::to_vec(&request).unwrap().as_slice(),
            None,
        )
        .await
        .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().all(|item| item["type"] != "reasoning"));
        assert!(input.iter().all(|item| item["type"] != "item_reference"));
        assert!(input
            .iter()
            .all(|item| item["type"] != "function_call_output"));
    }

    #[tokio::test]
    async fn degrades_image_input_to_text_for_excel_upstream() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({
            "input":[{"type":"message","role":"user","content":[
                {"type":"input_text","text":"Describe this"},
                {"type":"input_image","image_url":"data:image/png;base64,AAAA","detail":"high"}
            ]}]
        });
        let prepared = prepare_request(
            &state,
            serde_json::to_vec(&request).unwrap().as_slice(),
            None,
        )
        .await
        .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        let content = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "user")
            .unwrap()["content"]
            .as_array()
            .unwrap();
        assert!(content.iter().all(|part| part["type"] == "input_text"));
        assert!(content[1]["text"].as_str().unwrap().contains("Image input"));
        assert!(serde_json::to_string(&body).unwrap().contains("AAAA") == false);
    }

    #[tokio::test]
    async fn uses_output_text_for_assistant_history() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},
                {"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}
            ]
        });
        let prepared = prepare_request(&state, &serde_json::to_vec(&request).unwrap(), None)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        let assistant = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["role"] == "assistant")
            .unwrap();
        assert_eq!(assistant["content"][0]["type"], "output_text");
    }

    #[tokio::test]
    async fn drops_orphaned_tool_outputs() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({
            "input":[{"type":"function_call_output","call_id":"missing","output":"ok"}]
        });
        let prepared = prepare_request(&state, &serde_json::to_vec(&request).unwrap(), None)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert!(body["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "function_call_output"));
    }

    #[tokio::test]
    async fn rehydrates_cached_call_for_result_only_reconnect() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({
            "prompt_cache_key":"stable-thread",
            "input":[{"role":"user","content":[{"type":"input_text","text":"weather"}]}],
            "tools":[{"type":"function","name":"get_weather","parameters":{"type":"object"}}]
        });
        let prepared = prepare_request(&state, &serde_json::to_vec(&request).unwrap(), None)
            .await
            .unwrap();
        let native = json!({
            "type":"function_call","id":"fc_native","call_id":"call_reconnect",
            "name":"run_officejs","arguments":serde_json::to_string(&json!({
                "code":serde_json::to_string(&json!({"name":"get_weather","arguments":{"city":"Tokyo"}})).unwrap()
            })).unwrap()
        });
        transform_event(
            &state,
            &prepared.lineage,
            &json!({"type":"response.output_item.added","item":native.clone()}),
        )
        .await;
        transform_event(
            &state,
            &prepared.lineage,
            &json!({"type":"response.output_item.done","item":native}),
        )
        .await;
        let reconnect = json!({
            "prompt_cache_key":"stable-thread",
            "input":[{"type":"function_call_output","call_id":"call_reconnect","output":"18C"}]
        });
        let replay = prepare_request(&state, &serde_json::to_vec(&reconnect).unwrap(), None)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&replay.body).unwrap();
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().any(|item| item["type"] == "function_call"));
        assert!(input
            .iter()
            .any(|item| item["type"] == "function_call_output"));
    }

    #[test]
    fn websocket_history_expands_incremental_response_create() {
        let mut history = WsHistory::default();
        let first = json!({"type":"response.create","input":[{"role":"user","content":[{"type":"input_text","text":"one"}]}]});
        let response = json!({"id":"resp_1","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}]});
        history.remember(&first, &response);
        let expanded = history
            .expand(r#"{"type":"response.create","previous_response_id":"resp_1","input":[{"role":"user","content":[{"type":"input_text","text":"two"}]}]}"#)
            .unwrap();
        assert!(expanded.get("previous_response_id").is_none());
        assert_eq!(expanded["input"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn websocket_history_rejects_unknown_previous_response() {
        let history = WsHistory::default();
        let error = history
            .expand(r#"{"type":"response.create","previous_response_id":"missing","input":[]}"#)
            .unwrap_err();
        assert!(error.to_string().contains("previous_response_not_found"));
    }

    #[tokio::test]
    async fn state_lineage_isolated_by_account_and_model() {
        let state = Arc::new(Mutex::new(BpsState::default()));
        let request = json!({"model":"gpt-6-astra","input":"hello"});
        let first = prepare_request(
            &state,
            serde_json::to_vec(&request).unwrap().as_slice(),
            Some("account-a"),
        )
        .await
        .unwrap();
        let second = prepare_request(
            &state,
            serde_json::to_vec(&request).unwrap().as_slice(),
            Some("account-b"),
        )
        .await
        .unwrap();
        assert_ne!(first.lineage, second.lineage);
    }
}

/// Per-socket history: Codex WS v2 sends only the input delta after a response.
#[derive(Default)]
pub struct WsHistory {
    histories: HashMap<String, Vec<Value>>,
}
impl WsHistory {
    pub fn expand(&self, frame: &str) -> Result<Value> {
        let mut body: Value = serde_json::from_str(frame)?;
        if body.get("type").and_then(Value::as_str) != Some("response.create") {
            anyhow::bail!("unsupported websocket event");
        }
        if let Some(previous) = body.get("previous_response_id").and_then(Value::as_str) {
            let mut history = self
                .histories
                .get(previous)
                .cloned()
                .ok_or_else(|| anyhow!("previous_response_not_found"))?;
            history.extend(
                body.get("input")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            body["input"] = json!(history);
        }
        body.as_object_mut().unwrap().remove("previous_response_id");
        Ok(body)
    }
    pub fn remember(&mut self, request: &Value, response: &Value) {
        if let Some(id) = response.get("id").and_then(Value::as_str) {
            let mut items = request
                .get("input")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            items.extend(
                response
                    .get("output")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            self.histories.clear(); // Codex continues from the most recent completed response.
            self.histories.insert(id.to_owned(), items);
        }
    }
}
