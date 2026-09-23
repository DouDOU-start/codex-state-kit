//! Mihomo runs as a sidecar. This process only writes a local mixed-port config
//! and talks to the external controller. Protocol handshakes stay inside Mihomo.
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::settings::Settings;

/// The select group Kit writes into the core config; its choice is `mihomo_node`.
pub const GROUP: &str = "Kit";
const MAX_PROXIES: usize = 256;
const MAX_BODY: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub struct MihomoPaths {
    pub bundled_binary: PathBuf,
    pub data_dir: PathBuf,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MihomoStatus {
    pub available: bool,
    pub phase: String,
    pub proxy_url: Option<String>,
    pub selected: Option<String>,
    pub nodes: Vec<String>,
    pub groups: Vec<ProxyGroup>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyGroup {
    pub name: String,
    pub group_type: String,
    pub now: Option<String>,
    pub all: Vec<ProxyGroupNode>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyGroupNode {
    pub name: String,
    pub node_type: String,
    pub delay: Option<u64>,
    pub udp: bool,
}

#[derive(Clone, Debug)]
pub struct ProxyNode {
    pub name: String,
    pub spec: serde_yaml::Value,
}

pub struct SubscriptionConfig {
    pub nodes: Vec<ProxyNode>,
    pub groups: Vec<serde_yaml::Value>,
    pub rules: Vec<serde_yaml::Value>,
}

struct Inner {
    view: MihomoStatus,
    child: Option<OwnedChild>,
    proxy_url: Option<String>,
    controller: Option<String>,
    secret: Option<String>,
}

pub struct MihomoRuntime {
    paths: Option<MihomoPaths>,
    inner: Mutex<Inner>,
    operation: tokio::sync::Mutex<()>,
}

impl Default for MihomoRuntime {
    fn default() -> Self {
        Self::new(None)
    }
}

impl MihomoRuntime {
    pub fn new(paths: Option<MihomoPaths>) -> Self {
        let available = paths
            .as_ref()
            .is_some_and(|paths| paths.bundled_binary.is_file());
        Self {
            paths,
            inner: Mutex::new(Inner {
                view: MihomoStatus {
                    available,
                    phase: "stopped".into(),
                    proxy_url: None,
                    selected: None,
                    nodes: Vec::new(),
                    groups: Vec::new(),
                    error: None,
                },
                child: None,
                proxy_url: None,
                controller: None,
                secret: None,
            }),
            operation: tokio::sync::Mutex::new(()),
        }
    }

    pub fn status(&self) -> MihomoStatus {
        let mut inner = self.inner.lock().expect("mihomo state");
        let exited = inner
            .child
            .as_mut()
            .is_some_and(|child| !matches!(child.child.try_wait(), Ok(None)));
        if exited {
            inner.child.take();
            inner.proxy_url = None;
            inner.controller = None;
            inner.secret = None;
            inner.view.phase = "error".into();
            inner.view.proxy_url = None;
            inner.view.error = Some("Mihomo 内核已退出，请重新连接。".into());
        }
        inner.view.clone()
    }

    pub async fn probe_delays(&self, target: &str) -> Result<Vec<crate::latency::LatencySample>> {
        let group = {
            let inner = self.inner.lock().expect("mihomo state");
            inner
                .view
                .groups
                .iter()
                .find(|group| group.group_type == "select" || group.group_type == "url-test")
                .map(|group| group.name.clone())
                .unwrap_or_else(|| GROUP.to_string())
        };
        self.probe_group_delays(&group, target).await
    }

    pub async fn list_groups(&self) -> Result<Vec<ProxyGroup>> {
        self.refresh_groups().await
    }

    pub async fn select_in_group(&self, group: &str, node: &str) -> Result<()> {
        let (controller, secret) = self.controller_auth()?;
        select_node(&controller, &secret, group, node).await?;
        let _ = self.refresh_groups().await;
        Ok(())
    }

    pub async fn probe_group_delays(
        &self,
        group: &str,
        target: &str,
    ) -> Result<Vec<crate::latency::LatencySample>> {
        let (controller, secret, names) = {
            let inner = self.inner.lock().expect("mihomo state");
            if inner.view.phase != "connected" {
                bail!("订阅节点尚未就绪");
            }
            let names = inner
                .view
                .groups
                .iter()
                .find(|item| item.name == group)
                .map(|item| item.all.iter().map(|node| node.name.clone()).collect())
                .unwrap_or_else(|| inner.view.nodes.clone());
            (
                inner.controller.clone().context("订阅节点尚未就绪")?,
                inner.secret.clone().context("订阅节点尚未就绪")?,
                names,
            )
        };
        if names.is_empty() {
            bail!("订阅里没有可用节点");
        }
        let url = crate::latency::group_delay_url(&controller, group, target)?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(12))
            .build()
            .context("无法创建节点检测客户端")?;
        let response = client
            .get(url)
            .header("Authorization", format!("Bearer {secret}"))
            .send()
            .await
            .context("无法检测节点延迟")?;
        if !response.status().is_success() {
            bail!("节点延迟检测失败");
        }
        let body = response.json().await.context("无法读取延迟结果")?;
        let samples = crate::latency::samples_from_group_delays(&names, &body);
        self.apply_delay_samples(group, &samples);
        Ok(samples)
    }

    fn controller_auth(&self) -> Result<(String, String)> {
        let inner = self.inner.lock().expect("mihomo state");
        if inner.view.phase != "connected" {
            bail!("订阅节点尚未就绪");
        }
        Ok((
            inner.controller.clone().context("订阅节点尚未就绪")?,
            inner.secret.clone().context("订阅节点尚未就绪")?,
        ))
    }

    async fn refresh_groups(&self) -> Result<Vec<ProxyGroup>> {
        let (controller, secret) = self.controller_auth()?;
        let groups = fetch_groups(&controller, &secret).await?;
        let mut inner = self.inner.lock().expect("mihomo state");
        apply_groups(&mut inner.view, &groups);
        Ok(groups)
    }

    fn apply_delay_samples(&self, group: &str, samples: &[crate::latency::LatencySample]) {
        let mut inner = self.inner.lock().expect("mihomo state");
        let Some(target) = inner.view.groups.iter_mut().find(|item| item.name == group) else {
            return;
        };
        for sample in samples {
            if let Some(node) = target.all.iter_mut().find(|node| node.name == sample.name) {
                node.delay = sample.delay_ms;
            }
        }
    }

    pub fn proxy_url(&self) -> Result<String> {
        let inner = self.inner.lock().expect("mihomo state");
        if let Some(url) = &inner.proxy_url {
            return Ok(url.clone());
        }
        bail!(
            "{}",
            inner
                .view
                .error
                .clone()
                .unwrap_or_else(|| "订阅节点尚未就绪。".into())
        )
    }

    pub async fn stop(&self) -> MihomoStatus {
        let _operation = self.operation.lock().await;
        self.stop_inner();
        self.status()
    }

    pub async fn start(&self, settings: &Settings) -> Result<MihomoStatus> {
        let _operation = self.operation.lock().await;
        self.stop_inner();
        {
            let mut inner = self.inner.lock().expect("mihomo state");
            inner.view.phase = "starting".into();
            inner.view.error = None;
        }
        if let Err(err) = self.start_inner(settings).await {
            self.stop_inner();
            let message = format!("{err:#}");
            let mut inner = self.inner.lock().expect("mihomo state");
            inner.view.phase = "error".into();
            inner.view.error = Some(message.clone());
            bail!("{message}");
        }
        Ok(self.status())
    }

    fn stop_inner(&self) {
        let mut inner = self.inner.lock().expect("mihomo state");
        inner.child.take();
        inner.proxy_url = None;
        inner.controller = None;
        inner.secret = None;
        inner.view.phase = "stopped".into();
        inner.view.proxy_url = None;
        inner.view.selected = None;
        inner.view.nodes.clear();
        inner.view.groups.clear();
        inner.view.error = None;
    }

    async fn start_inner(&self, settings: &Settings) -> Result<()> {
        let paths = self
            .paths
            .as_ref()
            .context("此构建未配置 Mihomo 数据目录")?;
        fs::create_dir_all(&paths.data_dir).context("无法创建 Mihomo 数据目录")?;
        let binary = bundled_binary(paths)?;
        let body = load_subscription(&settings.mihomo_subscription).await?;
        let subscription = parse_subscription(&body)?;
        if subscription.nodes.is_empty() {
            bail!("订阅里没有可用节点");
        }
        let names: Vec<String> = subscription
            .nodes
            .iter()
            .map(|node| node.name.clone())
            .collect();
        let wanted = settings.mihomo_node.trim().to_string();
        let mixed = free_port()?;
        let controller_port = free_port()?;
        let secret = format!("{:032x}", rand::random::<u128>());
        let controller = format!("127.0.0.1:{controller_port}");
        let config = render_config(&subscription, mixed, &controller, &secret);
        let config_path = paths.data_dir.join("config.yaml");
        fs::write(&config_path, config).context("无法写入 Mihomo 配置")?;
        let log =
            File::create(paths.data_dir.join("mihomo.log")).context("无法写入 Mihomo 日志")?;
        let mut command = sidecar_command(&binary, &log, &paths.data_dir)?;
        command
            .arg("-d")
            .arg(&paths.data_dir)
            .arg("-f")
            .arg(&config_path);
        let mut child = OwnedChild::spawn(&mut command)?;
        let proxy_url = format!("http://127.0.0.1:{mixed}");
        {
            let mut inner = self.inner.lock().expect("mihomo state");
            inner.proxy_url = Some(proxy_url.clone());
            inner.controller = Some(controller.clone());
            inner.secret = Some(secret.clone());
            inner.view.phase = "connecting".into();
            inner.view.nodes = names.clone();
            inner.view.available = true;
        }
        // This is a local sidecar endpoint. Do not route the readiness probe
        // through HTTP(S)_PROXY/ALL_PROXY inherited from the desktop process.
        wait_until_ready(
            &controller,
            &secret,
            &mut child,
            &paths.data_dir.join("mihomo.log"),
        )
        .await?;
        let chosen = {
            let name = (!wanted.is_empty() && names.iter().any(|item| item == &wanted))
                .then(|| wanted.clone())
                .or_else(|| names.first().cloned());
            if let Some(name) = name {
                select_node(&controller, &secret, GROUP, &name).await?;
                Some(name)
            } else {
                None
            }
        };
        let groups = fetch_groups(&controller, &secret).await.unwrap_or_default();
        let mut inner = self.inner.lock().expect("mihomo state");
        inner.child = Some(child);
        inner.view.phase = "connected".into();
        inner.view.proxy_url = Some(proxy_url);
        inner.view.nodes = names;
        inner.view.error = None;
        inner.view.available = true;
        if groups.is_empty() {
            inner.view.selected = chosen;
            inner.view.groups.clear();
        } else {
            apply_groups(&mut inner.view, &groups);
        }
        Ok(())
    }
}

fn bundled_binary(paths: &MihomoPaths) -> Result<PathBuf> {
    if !paths.bundled_binary.is_file() {
        bail!("内核文件缺失，请使用完整安装包。")
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let metadata = fs::metadata(&paths.bundled_binary).context("无法读取 Mihomo 内核权限")?;
        if metadata.permissions().mode() & 0o111 == 0 {
            // Some macOS archive and app-bundle copy steps lose the executable
            // bit. Keep the bundled resource untouched and run a writable copy.
            let fallback = paths.data_dir.join(".mihomo");
            fs::copy(&paths.bundled_binary, &fallback).context("无法准备 Mihomo 内核")?;
            let mut permissions = fs::metadata(&fallback)
                .context("无法读取 Mihomo 内核副本权限")?
                .permissions();
            permissions.set_mode(permissions.mode() | 0o755);
            fs::set_permissions(&fallback, permissions).context("无法设置 Mihomo 内核执行权限")?;
            return Ok(fallback);
        }
    }
    Ok(paths.bundled_binary.clone())
}

async fn load_subscription(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("请填写订阅地址");
    }
    if raw.len() > MAX_BODY {
        bail!("订阅内容过长");
    }
    let path = Path::new(raw);
    if path.is_file() {
        let text = fs::read_to_string(path).context("无法读取订阅文件")?;
        if text.len() > MAX_BODY {
            bail!("订阅文件过长");
        }
        return Ok(text);
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("无法创建订阅客户端")?;
        let text = client
            .get(raw)
            .header("User-Agent", "codex-state-kit")
            .send()
            .await
            .context("订阅下载失败")?
            .error_for_status()
            .context("订阅下载被拒绝")?
            .text()
            .await
            .context("订阅内容读取失败")?;
        if text.len() > MAX_BODY {
            bail!("订阅内容过长");
        }
        return Ok(text);
    }
    Ok(raw.to_string())
}

pub fn parse_subscription(raw: &str) -> Result<SubscriptionConfig> {
    let text = raw.trim();
    if text.is_empty() {
        bail!("订阅为空");
    }
    if let Some(config) = yaml_subscription(text) {
        if !config.nodes.is_empty() {
            return Ok(config);
        }
    }
    if let Some(decoded) = decode_text(text) {
        if let Some(config) = yaml_subscription(&decoded) {
            if !config.nodes.is_empty() {
                return Ok(config);
            }
        }
        let nodes = uri_lines(&decoded)?;
        if !nodes.is_empty() {
            return Ok(nodes_only(nodes));
        }
    }
    let nodes = uri_lines(text)?;
    if nodes.is_empty() {
        bail!("订阅里没有识别到节点");
    }
    Ok(nodes_only(nodes))
}

fn nodes_only(nodes: Vec<ProxyNode>) -> SubscriptionConfig {
    SubscriptionConfig {
        nodes,
        groups: Vec::new(),
        rules: Vec::new(),
    }
}

fn yaml_subscription(text: &str) -> Option<SubscriptionConfig> {
    let value: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    let list = value
        .get("proxies")
        .and_then(serde_yaml::Value::as_sequence)
        .or_else(|| value.as_sequence())?;
    Some(SubscriptionConfig {
        nodes: unique_nodes(list.iter().filter_map(node_from_yaml)),
        groups: sequence_values(&value, "proxy-groups"),
        rules: sequence_values(&value, "rules"),
    })
}

fn sequence_values(value: &serde_yaml::Value, key: &str) -> Vec<serde_yaml::Value> {
    value
        .get(key)
        .and_then(serde_yaml::Value::as_sequence)
        .cloned()
        .unwrap_or_default()
}

fn node_from_yaml(value: &serde_yaml::Value) -> Option<ProxyNode> {
    let name = value.get("name")?.as_str()?.trim();
    if name.is_empty()
        || value
            .get("type")
            .and_then(serde_yaml::Value::as_str)
            .is_none()
    {
        return None;
    }
    Some(ProxyNode {
        name: name.to_string(),
        spec: value.clone(),
    })
}

fn uri_lines(text: &str) -> Result<Vec<ProxyNode>> {
    let mut nodes = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(node) = parse_uri(line) {
            nodes.push(node);
        }
        if nodes.len() >= MAX_PROXIES {
            break;
        }
    }
    Ok(unique_nodes(nodes.into_iter()))
}

fn unique_nodes(nodes: impl Iterator<Item = ProxyNode>) -> Vec<ProxyNode> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for mut node in nodes {
        if unique.len() >= MAX_PROXIES {
            break;
        }
        let mut name = node.name.clone();
        let mut index = 2;
        while !seen.insert(name.clone()) {
            name = format!("{}-{index}", node.name);
            index += 1;
        }
        if name != node.name {
            if let Some(mapping) = node.spec.as_mapping_mut() {
                mapping.insert(
                    serde_yaml::Value::String("name".into()),
                    serde_yaml::Value::String(name.clone()),
                );
            }
            node.name = name;
        }
        unique.push(node);
    }
    unique
}

fn parse_uri(line: &str) -> Option<ProxyNode> {
    let (scheme, rest) = line.split_once("://")?;
    match scheme.to_ascii_lowercase().as_str() {
        "ss" => parse_shadowsocks(rest),
        "vmess" => parse_vmess(rest),
        "vless" => parse_vless(rest),
        "trojan" => parse_trojan(rest),
        "hysteria2" | "hy2" => parse_hysteria2(rest),
        "tuic" => parse_tuic(rest),
        _ => None,
    }
}

fn parse_shadowsocks(rest: &str) -> Option<ProxyNode> {
    let (body, name) = split_name(rest);
    if let Some((userinfo, hostport)) = body.split_once('@') {
        let decoded = String::from_utf8(b64(userinfo)?).ok()?;
        let (cipher, password) = decoded.split_once(':')?;
        let (server, port) = split_host_port(hostport)?;
        let display = name.unwrap_or_else(|| server.to_string());
        return Some(ss_node(&display, server, port, cipher, password));
    }
    let decoded = String::from_utf8(b64(body)?).ok()?;
    let (method, rest) = decoded.split_once(':')?;
    let (password, hostport) = rest.rsplit_once('@')?;
    let (server, port) = split_host_port(hostport)?;
    let display = name.unwrap_or_else(|| server.to_string());
    Some(ss_node(&display, server, port, method, password))
}

fn ss_node(name: &str, server: &str, port: u16, cipher: &str, password: &str) -> ProxyNode {
    mapping_node(
        name,
        "ss",
        &[
            ("server", yaml_str(server)),
            ("port", yaml_int(port)),
            ("cipher", yaml_str(cipher)),
            ("password", yaml_str(password)),
            ("udp", serde_yaml::Value::Bool(true)),
        ],
    )
}

fn parse_vmess(rest: &str) -> Option<ProxyNode> {
    let (body, _) = split_name(rest);
    let json: JsonValue = serde_json::from_slice(&b64(body)?).ok()?;
    let server = json.get("add")?.as_str()?.trim();
    let port = json_port(json.get("port")?)?;
    let uuid = json.get("id")?.as_str()?.trim();
    let name = json
        .get("ps")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(server);
    let mut fields = vec![
        ("server", yaml_str(server)),
        ("port", yaml_int(port)),
        ("uuid", yaml_str(uuid)),
        (
            "alterId",
            yaml_int(json_port(json.get("aid").unwrap_or(&JsonValue::from(0))).unwrap_or(0)),
        ),
        (
            "cipher",
            yaml_str(
                json.get("scy")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("auto"),
            ),
        ),
        ("udp", serde_yaml::Value::Bool(true)),
    ];
    let network = json.get("net").and_then(JsonValue::as_str).unwrap_or("tcp");
    if network != "tcp" {
        fields.push(("network", yaml_str(network)));
    }
    if json.get("tls").and_then(JsonValue::as_str) == Some("tls") {
        fields.push(("tls", serde_yaml::Value::Bool(true)));
        if let Some(sni) = json
            .get("sni")
            .and_then(JsonValue::as_str)
            .filter(|sni| !sni.is_empty())
        {
            fields.push(("servername", yaml_str(sni)));
        }
    }
    Some(mapping_node(name, "vmess", &fields))
}

fn parse_vless(rest: &str) -> Option<ProxyNode> {
    let url = url::Url::parse(&format!("vless://{rest}")).ok()?;
    let uuid = url.username();
    if uuid.is_empty() {
        return None;
    }
    let server = url.host_str()?;
    let port = url.port()?;
    let name = url_name(&url).unwrap_or(server);
    let mut fields = vec![
        ("server", yaml_str(server)),
        ("port", yaml_int(port)),
        ("uuid", yaml_str(uuid)),
        ("udp", serde_yaml::Value::Bool(true)),
    ];
    push_stream_fields(&mut fields, &url);
    Some(mapping_node(name, "vless", &fields))
}

fn parse_trojan(rest: &str) -> Option<ProxyNode> {
    let url = url::Url::parse(&format!("trojan://{rest}")).ok()?;
    let password = urlencoding_username(&url)?;
    let server = url.host_str()?;
    let port = url.port().unwrap_or(443);
    let name = url_name(&url).unwrap_or(server);
    let mut fields = vec![
        ("server", yaml_str(server)),
        ("port", yaml_int(port)),
        ("password", yaml_str(&password)),
        ("udp", serde_yaml::Value::Bool(true)),
    ];
    push_stream_fields(&mut fields, &url);
    Some(mapping_node(name, "trojan", &fields))
}

fn parse_hysteria2(rest: &str) -> Option<ProxyNode> {
    let url = url::Url::parse(&format!("hysteria2://{rest}")).ok()?;
    let server = url.host_str()?;
    let port = url.port().unwrap_or(443);
    let name = url_name(&url).unwrap_or(server);
    let mut fields = vec![("server", yaml_str(server)), ("port", yaml_int(port))];
    if let Some(password) = urlencoding_username(&url) {
        if !password.is_empty() {
            fields.push(("password", yaml_str(&password)));
        }
    }
    if let Some(sni) = url
        .query_pairs()
        .find(|(key, _)| key == "sni")
        .map(|(_, value)| value.into_owned())
    {
        fields.push(("sni", yaml_str(&sni)));
    }
    Some(mapping_node(name, "hysteria2", &fields))
}

fn parse_tuic(rest: &str) -> Option<ProxyNode> {
    let url = url::Url::parse(&format!("tuic://{rest}")).ok()?;
    let uuid = url.username();
    let password = url.password()?;
    let server = url.host_str()?;
    let port = url.port().unwrap_or(443);
    let name = url_name(&url).unwrap_or(server);
    Some(mapping_node(
        name,
        "tuic",
        &[
            ("server", yaml_str(server)),
            ("port", yaml_int(port)),
            ("uuid", yaml_str(uuid)),
            ("password", yaml_str(password)),
            ("udp", serde_yaml::Value::Bool(true)),
        ],
    ))
}

fn push_stream_fields(fields: &mut Vec<(&str, serde_yaml::Value)>, url: &url::Url) {
    let query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let get = |name: &str| {
        query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    let network = get("type").unwrap_or("tcp");
    if network != "tcp" {
        fields.push(("network", yaml_str(network)));
    }
    let security = get("security").unwrap_or("");
    if security == "tls"
        || security == "reality"
        || get("tls").is_some_and(|value| value == "1" || value == "true")
    {
        fields.push(("tls", serde_yaml::Value::Bool(true)));
    }
    if let Some(sni) = get("sni").filter(|sni| !sni.is_empty()) {
        fields.push(("servername", yaml_str(sni)));
    }
}

fn mapping_node(name: &str, kind: &str, fields: &[(&str, serde_yaml::Value)]) -> ProxyNode {
    let mut mapping = serde_yaml::Mapping::new();
    mapping.insert(yaml_str("name"), yaml_str(name));
    mapping.insert(yaml_str("type"), yaml_str(kind));
    for (key, value) in fields {
        mapping.insert(yaml_str(key), value.clone());
    }
    ProxyNode {
        name: name.to_string(),
        spec: serde_yaml::Value::Mapping(mapping),
    }
}

fn yaml_str(value: &str) -> serde_yaml::Value {
    serde_yaml::Value::String(value.to_string())
}

fn yaml_int(value: u16) -> serde_yaml::Value {
    serde_yaml::Value::Number(i64::from(value).into())
}

fn split_name(rest: &str) -> (&str, Option<String>) {
    match rest.rsplit_once('#') {
        Some((body, name)) => {
            let name = percent_decode(name);
            let name = name.trim().to_string();
            (body, (!name.is_empty()).then_some(name))
        }
        None => (rest, None),
    }
}

fn split_host_port(hostport: &str) -> Option<(&str, u16)> {
    let (host, port) = hostport.rsplit_once(':')?;
    let host = host.trim_matches(['[', ']']);
    let port = port.parse().ok()?;
    (!host.is_empty()).then_some((host, port))
}

fn url_name(url: &url::Url) -> Option<&str> {
    let fragment = url.fragment()?.trim();
    (!fragment.is_empty()).then_some(fragment)
}

fn urlencoding_username(url: &url::Url) -> Option<String> {
    let name = url.username();
    (!name.is_empty()).then(|| percent_decode(name).to_string())
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::new();
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or(""),
                16,
            ) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| value.to_string())
}

fn json_port(value: &JsonValue) -> Option<u16> {
    value
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .or_else(|| value.as_str().and_then(|port| port.parse().ok()))
}

fn b64(raw: &str) -> Option<Vec<u8>> {
    let raw = raw.trim();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(raw))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(raw))
        .ok()
}

fn decode_text(raw: &str) -> Option<String> {
    if raw.lines().count() != 1 || raw.contains(' ') {
        return None;
    }
    String::from_utf8(b64(raw)?).ok()
}

pub fn render_config(
    config: &SubscriptionConfig,
    mixed_port: u16,
    controller: &str,
    secret: &str,
) -> String {
    let mut map = serde_yaml::Mapping::new();
    map.insert(yaml_str("mixed-port"), yaml_int(mixed_port));
    map.insert(yaml_str("allow-lan"), serde_yaml::Value::Bool(false));
    map.insert(yaml_str("bind-address"), yaml_str("127.0.0.1"));
    map.insert(yaml_str("mode"), yaml_str("rule"));
    map.insert(yaml_str("log-level"), yaml_str("warning"));
    map.insert(yaml_str("ipv6"), serde_yaml::Value::Bool(false));
    map.insert(yaml_str("find-process-mode"), yaml_str("off"));
    map.insert(yaml_str("external-controller"), yaml_str(controller));
    map.insert(yaml_str("secret"), yaml_str(secret));
    map.insert(
        yaml_str("proxies"),
        serde_yaml::Value::Sequence(config.nodes.iter().map(|node| node.spec.clone()).collect()),
    );
    let names: Vec<serde_yaml::Value> = config
        .nodes
        .iter()
        .map(|node| yaml_str(&node.name))
        .collect();
    let mut group = serde_yaml::Mapping::new();
    group.insert(yaml_str("name"), yaml_str(GROUP));
    group.insert(yaml_str("type"), yaml_str("select"));
    group.insert(yaml_str("proxies"), serde_yaml::Value::Sequence(names));
    map.insert(
        yaml_str("proxy-groups"),
        serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(group)]),
    );
    map.insert(
        yaml_str("rules"),
        serde_yaml::Value::Sequence(vec![yaml_str(&format!("MATCH,{GROUP}"))]),
    );
    serde_yaml::to_string(&serde_yaml::Value::Mapping(map)).unwrap_or_default()
}

fn apply_groups(view: &mut MihomoStatus, groups: &[ProxyGroup]) {
    view.groups = groups.to_vec();
    if let Some(primary) = groups.iter().find(|group| group.group_type == "select") {
        view.selected = primary.now.clone();
        view.nodes = primary.all.iter().map(|node| node.name.clone()).collect();
    }
}

async fn fetch_groups(controller: &str, secret: &str) -> Result<Vec<ProxyGroup>> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()
        .context("无法创建节点检测客户端")?;
    let response = client
        .get(format!("http://{controller}/proxies"))
        .header("Authorization", format!("Bearer {secret}"))
        .send()
        .await
        .context("无法读取 Mihomo 代理组")?;
    if !response.status().is_success() {
        bail!("Mihomo 拒绝读取代理组");
    }
    let body = response.json().await.context("无法解析 Mihomo 代理组")?;
    Ok(parse_groups_from_api(&body))
}

fn parse_groups_from_api(body: &JsonValue) -> Vec<ProxyGroup> {
    let Some(proxies) = body.get("proxies").and_then(JsonValue::as_object) else {
        return Vec::new();
    };
    let mut groups = Vec::new();
    for (name, value) in proxies {
        if name == "GLOBAL" || name == "COMPATIBLE" {
            continue;
        }
        let Some(group_type) = value
            .get("type")
            .and_then(JsonValue::as_str)
            .and_then(normalize_group_type)
        else {
            continue;
        };
        let all = value
            .get("all")
            .and_then(JsonValue::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(JsonValue::as_str)
                    .map(|node_name| {
                        let node = proxies.get(node_name);
                        ProxyGroupNode {
                            name: node_name.to_string(),
                            node_type: node
                                .and_then(|node| node.get("type"))
                                .and_then(JsonValue::as_str)
                                .unwrap_or("unknown")
                                .to_string(),
                            delay: node.and_then(node_delay),
                            udp: node
                                .and_then(|node| node.get("udp"))
                                .and_then(JsonValue::as_bool)
                                .unwrap_or(false),
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        groups.push(ProxyGroup {
            name: name.clone(),
            group_type: group_type.to_string(),
            now: value
                .get("now")
                .and_then(JsonValue::as_str)
                .map(str::to_string),
            all,
        });
    }
    groups
}

fn normalize_group_type(kind: &str) -> Option<&'static str> {
    match kind {
        "Selector" => Some("select"),
        "URLTest" => Some("url-test"),
        "Fallback" => Some("fallback"),
        "LoadBalance" => Some("load-balance"),
        "Relay" => Some("relay"),
        _ => None,
    }
}

fn node_delay(node: &JsonValue) -> Option<u64> {
    node.get("history")
        .and_then(JsonValue::as_array)
        .and_then(|history| history.last())
        .and_then(|item| item.get("delay"))
        .and_then(JsonValue::as_u64)
        .filter(|delay| *delay > 0)
}

fn free_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").context("无法分配本地端口")?;
    Ok(listener.local_addr()?.port())
}

async fn wait_until_ready(
    controller: &str,
    secret: &str,
    child: &mut OwnedChild,
    log_path: &Path,
) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let url = format!("http://{controller}/version");
    loop {
        if client
            .get(&url)
            .header("Authorization", format!("Bearer {secret}"))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        if let Ok(Some(status)) = child.child.try_wait() {
            bail!("Mihomo 内核已退出（{status}）{}", log_detail(log_path));
        }
        if Instant::now() >= deadline {
            bail!("Mihomo 内核启动超时{}", log_detail(log_path));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn select_node(controller: &str, secret: &str, group: &str, name: &str) -> Result<()> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(5))
        .build()?;
    let group = crate::latency::encode_path_segment(group);
    let response = client
        .put(format!("http://{controller}/proxies/{group}"))
        .header("Authorization", format!("Bearer {secret}"))
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .context("无法切换 Mihomo 节点")?;
    if !response.status().is_success() {
        bail!("Mihomo 拒绝切换到所选节点");
    }
    Ok(())
}

fn tail_text(text: &str, max_chars: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let start = text
        .char_indices()
        .rev()
        .nth(max_chars.saturating_sub(1))
        .map_or(0, |(index, _)| index);
    format!("…{}", &text[start..])
}

fn log_detail(log_path: &Path) -> String {
    let tail = fs::read_to_string(log_path)
        .ok()
        .map(|text| tail_text(&text, 2_000))
        .filter(|text| !text.is_empty());
    match tail {
        Some(text) => format!("；日志文件：{}；日志尾部：{text}", log_path.display()),
        None => format!("；日志文件：{}", log_path.display()),
    }
}

fn sidecar_command(binary: &Path, log: &File, working_dir: &Path) -> Result<Command> {
    let mut command = Command::new(binary);
    command
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log.try_clone()?)
        .current_dir(working_dir);
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
        command.creation_flags(0x08000000);
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
        let mut child = command.spawn().context("无法启动 Mihomo 内核")?;
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
                    .context("无法管理 Mihomo 子进程生命周期");
            }
            Ok(Self { _handle: owned })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_clash_yaml_and_renders_select_group() {
        let raw = r#"
proxies:
  - name: alpha
    type: ss
    server: 1.2.3.4
    port: 8388
    cipher: aes-256-gcm
    password: secret
  - name: alpha
    type: trojan
    server: example.test
    port: 443
    password: other
"#;
        let parsed = parse_subscription(raw).unwrap();
        assert_eq!(parsed.nodes[0].name, "alpha");
        assert_eq!(parsed.nodes[1].name, "alpha-2");
        assert!(parsed.groups.is_empty());
        let config = render_config(&parsed, 17891, "127.0.0.1:17892", "secret");
        assert!(config.contains("mixed-port"));
        assert!(config.contains("name: Kit"));
        assert!(config.contains("MATCH,Kit"));
        assert!(!config.contains("tun:"));
        assert!(config.contains("password: secret"));
    }

    #[test]
    fn parses_base64_share_links() {
        let line = "ss://YWVzLTI1Ni1nY206cGFzcw@1.2.3.4:8388#home";
        let encoded = base64::engine::general_purpose::STANDARD.encode(line);
        let nodes = parse_subscription(&encoded).unwrap().nodes;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "home");
        let text = serde_yaml::to_string(&nodes[0].spec).unwrap();
        assert!(text.contains("aes-256-gcm"));
        assert!(text.contains("8388"));
    }

    #[test]
    fn parses_vmess_and_rejects_empty() {
        let payload = base64::engine::general_purpose::STANDARD.encode(
            br#"{"add":"vmess.example","port":"443","id":"11111111-1111-1111-1111-111111111111","aid":"0","net":"ws","tls":"tls","ps":"edge"}"#,
        );
        let nodes = parse_subscription(&format!("vmess://{payload}"))
            .unwrap()
            .nodes;
        assert_eq!(nodes[0].name, "edge");
        let text = serde_yaml::to_string(&nodes[0].spec).unwrap();
        assert!(text.contains("type: vmess"));
        assert!(text.contains("network: ws"));
        assert!(parse_subscription("   ").is_err());
        assert!(parse_subscription("not a subscription").is_err());
    }

    #[test]
    fn subscription_groups_collapse_into_kit() {
        let raw = r#"
proxies:
  - name: entry
    type: ss
    server: 1.2.3.4
    port: 8388
    cipher: aes-256-gcm
    password: secret
  - name: exit
    type: vless
    server: 5.6.7.8
    port: 443
    uuid: 11111111-1111-1111-1111-111111111111
    dialer-proxy: entry
proxy-groups:
  - name: 前置
    type: select
    proxies: [entry]
  - name: 代理
    type: select
    proxies: [exit, DIRECT]
rules:
  - DOMAIN-SUFFIX,openai.com,代理
  - MATCH,代理
"#;
        let parsed = parse_subscription(raw).unwrap();
        assert_eq!(parsed.groups.len(), 2);
        assert_eq!(parsed.rules.len(), 2);
        let config = render_config(&parsed, 17891, "127.0.0.1:17892", "secret");
        assert!(config.contains("name: Kit"));
        assert!(config.contains("MATCH,Kit"));
        assert!(config.contains("dialer-proxy: entry"));
        assert!(!config.contains("name: 前置"));
        assert!(!config.contains("DOMAIN-SUFFIX,openai.com,代理"));
    }

    #[test]
    fn parses_mihomo_proxy_groups() {
        let body = serde_json::json!({
            "proxies": {
                "GLOBAL": { "type": "Selector", "now": "代理", "all": ["代理"] },
                "前置": {
                    "type": "Selector",
                    "now": "entry",
                    "all": ["entry"]
                },
                "自动选择": {
                    "type": "URLTest",
                    "now": "exit",
                    "all": ["exit"]
                },
                "entry": { "type": "Shadowsocks", "udp": true, "history": [{ "delay": 166 }] },
                "exit": { "type": "Vless", "udp": true, "history": [{ "delay": 0 }] }
            }
        });
        let groups = parse_groups_from_api(&body);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "前置");
        assert_eq!(groups[0].group_type, "select");
        assert_eq!(groups[0].now.as_deref(), Some("entry"));
        assert_eq!(groups[0].all[0].delay, Some(166));
        assert_eq!(groups[1].group_type, "url-test");
        assert_eq!(groups[1].all[0].delay, None);
    }
}
