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
        let node = self.status().selected.context("尚未选择节点")?;
        Ok(vec![self.probe_node_delay(GROUP, &node, target).await?])
    }

    pub async fn probe_node_delay(
        &self,
        group: &str,
        node: &str,
        target: &str,
    ) -> Result<crate::latency::LatencySample> {
        let (controller, secret) = self.controller_auth()?;
        anyhow::ensure!(
            self.status()
                .groups
                .iter()
                .any(|g| g.name == group && g.all.iter().any(|n| n.name == node)),
            "节点不在当前分组中"
        );
        let url = crate::latency::node_delay_url(&controller, node, target)?;
        let client = crate::tls::http_client_builder()
            .no_proxy()
            .timeout(Duration::from_secs(6))
            .build()?;
        let result = async {
            let response = client
                .get(url)
                .bearer_auth(&secret)
                .send()
                .await
                .context("节点检测失败")?;
            anyhow::ensure!(response.status().is_success(), "节点超时或不可达");
            let body: JsonValue = response.json().await.context("节点返回无效结果")?;
            body.get("delay")
                .and_then(JsonValue::as_u64)
                .filter(|n| *n > 0)
                .context("节点超时或不可达")
        }
        .await;
        let sample = crate::latency::sample_from_result(node, result);
        if self
            .controller_auth()
            .is_ok_and(|auth| auth == (controller, secret))
        {
            self.apply_delay_samples(group, std::slice::from_ref(&sample));
        }
        Ok(sample)
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
        use futures_util::{stream, StreamExt};
        let _ = (controller, secret);
        let group = group.to_string();
        let target = target.to_string();
        let mut pending = stream::iter(names.into_iter().map(|node| {
            let group = group.clone();
            let target = target.clone();
            async move { self.probe_node_delay(&group, &node, &target).await }
        }))
        .buffer_unordered(4);
        let mut samples = Vec::new();
        while let Some(result) = pending.next().await {
            samples.push(result?);
        }
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
        let client = crate::tls::http_client_builder()
            .timeout(Duration::from_secs(20))
            .build()
            .context("无法创建订阅客户端")?;
        let text = client
            .get(raw)
            // Subscription services commonly return a URI list tailored to the
            // client. Mihomo understands AnyTLS, so request its profile instead
            // of receiving a list of schemes this importer cannot handle.
            .header("User-Agent", "clash.meta")
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
    if raw.len() > MAX_BODY {
        bail!("订阅内容过长");
    }
    let text = raw.trim().trim_start_matches('\u{feff}').trim();
    if text.is_empty() {
        bail!("订阅为空");
    }
    if let Some(config) = yaml_subscription(text)? {
        if !config.nodes.is_empty() {
            return Ok(config);
        }
    }
    if let Some(decoded) = decode_text(text) {
        let decoded = decoded.trim().trim_start_matches('\u{feff}').trim();
        if let Some(config) = yaml_subscription(decoded)? {
            if !config.nodes.is_empty() {
                return Ok(config);
            }
        }
        let nodes = uri_lines(decoded)?;
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

fn yaml_subscription(text: &str) -> Result<Option<SubscriptionConfig>> {
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
        return Ok(None);
    };
    let list = value
        .get("proxies")
        .and_then(serde_yaml::Value::as_sequence)
        .or_else(|| value.as_sequence());
    if list.is_none_or(Vec::is_empty) && value.get("proxy-providers").is_some() {
        bail!("暂不支持仅包含 proxy-providers 的订阅，请提供含 proxies 节点的 Clash/Mihomo 订阅");
    }
    let Some(list) = list else {
        return Ok(None);
    };
    Ok(Some(SubscriptionConfig {
        nodes: unique_nodes(list.iter().filter_map(node_from_yaml)),
        groups: sequence_values(&value, "proxy-groups"),
        rules: sequence_values(&value, "rules"),
    }))
}

fn sequence_values(value: &serde_yaml::Value, key: &str) -> Vec<serde_yaml::Value> {
    value
        .get(key)
        .and_then(serde_yaml::Value::as_sequence)
        .cloned()
        .unwrap_or_default()
}

fn node_from_yaml(value: &serde_yaml::Value) -> Option<ProxyNode> {
    let name = value.get("name")?.as_str()?;
    if name.trim().is_empty()
        || value
            .get("type")
            .and_then(serde_yaml::Value::as_str)
            .is_none()
    {
        return None;
    }
    if value
        .get("server")
        .and_then(serde_yaml::Value::as_str)
        .is_some_and(|server| server.trim().is_empty())
    {
        return None;
    }
    if value
        .get("port")
        .is_some_and(|port| yaml_port(port).is_none_or(|port| port == 0))
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
    let mut invalid_lines = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(node) = parse_uri(line) {
            nodes.push(node);
        } else if line.contains("://") && invalid_lines.len() < 8 {
            invalid_lines.push((index + 1).to_string());
        }
        if nodes.len() >= MAX_PROXIES {
            break;
        }
    }
    if nodes.is_empty() && !invalid_lines.is_empty() {
        // Report positions, never credentials or a subscription URL.
        bail!(
            "订阅里没有识别到节点：第 {} 行的分享链接格式不支持或参数无效；支持 ss、vmess、vless、trojan、hysteria2/hy2、anytls、tuic，也可提供 Clash/Mihomo YAML",
            invalid_lines.join("、")
        );
    }
    Ok(unique_nodes(nodes.into_iter()))
}

fn unique_nodes(nodes: impl Iterator<Item = ProxyNode>) -> Vec<ProxyNode> {
    let nodes: Vec<_> = nodes.take(MAX_PROXIES).collect();
    let original_names: std::collections::HashSet<_> = nodes
        .iter()
        .map(|node| node.name.trim().to_string())
        .collect();
    let mut seen: std::collections::HashSet<String> = [
        GROUP,
        "DIRECT",
        "REJECT",
        "REJECT-DROP",
        "COMPATIBLE",
        "PASS",
        "PASS-RULE",
        "GLOBAL",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    let mut renamed = std::collections::HashMap::new();
    let mut unique = Vec::new();
    for mut node in nodes {
        let base = node.name.trim();
        let mut name = base.to_string();
        let mut index = 2;
        while seen.contains(&name) || (name != base && original_names.contains(&name)) {
            name = format!("{base}-{index}");
            index += 1;
        }
        seen.insert(name.clone());
        renamed.entry(node.name.clone()).or_insert(name.clone());
        renamed.entry(base.to_string()).or_insert(name.clone());
        if let Some(mapping) = node.spec.as_mapping_mut() {
            mapping.insert(yaml_str("name"), yaml_str(&name));
        }
        node.name = name;
        unique.push(node);
    }
    for node in &mut unique {
        if let Some(target) = node
            .spec
            .get("dialer-proxy")
            .and_then(serde_yaml::Value::as_str)
        {
            if let Some(name) = renamed.get(target) {
                node.spec["dialer-proxy"] = yaml_str(name);
            }
        }
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
        "anytls" => parse_anytls(rest),
        "tuic" => parse_tuic(rest),
        _ => None,
    }
}

fn parse_shadowsocks(rest: &str) -> Option<ProxyNode> {
    let (body, name) = split_name(rest);
    // Do not strip '/' from a Base64 payload: it is part of the alphabet.
    let (body, outer_plugin) = body
        .split_once('?')
        .map_or((body, None), |(body, query)| (body, Some(query)));
    if let Some((userinfo, hostport)) = body.rsplit_once('@') {
        let (cipher, password) = if let Some((cipher, password)) = userinfo.split_once(':') {
            (percent_decode(cipher), percent_decode(password))
        } else {
            let decoded = String::from_utf8(b64(&percent_decode(userinfo))?).ok()?;
            let (cipher, password) = decoded.split_once(':')?;
            (cipher.to_string(), password.to_string())
        };
        if cipher.is_empty() {
            return None;
        }
        let (hostport, inline_plugin) = split_uri_query(hostport);
        let (server, port) = split_host_port(hostport)?;
        let display = name.unwrap_or_else(|| server.to_string());
        let mut node = ss_node(&display, server, port, &cipher, &password);
        apply_shadowsocks_plugin(&mut node, plugin_query(outer_plugin, inline_plugin));
        return Some(node);
    }
    let decoded = String::from_utf8(decode_b64_payload(body)?).ok()?;
    let (method, rest) = decoded.split_once(':')?;
    let (password, hostport) = rest.rsplit_once('@')?;
    let (hostport, inline_plugin) = split_uri_query(hostport);
    let (server, port) = split_host_port(hostport)?;
    let display = name.unwrap_or_else(|| server.to_string());
    let mut node = ss_node(&display, server, port, method, password);
    apply_shadowsocks_plugin(&mut node, plugin_query(outer_plugin, inline_plugin));
    Some(node)
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
    let (body, fragment_name) = split_name(rest);
    let json: JsonValue = serde_json::from_slice(&decode_b64_payload(body)?).ok()?;
    let server = json.get("add")?.as_str()?.trim();
    if server.is_empty() {
        return None;
    }
    let port = json_port(json.get("port")?)?;
    if port == 0 {
        return None;
    }
    let uuid = json.get("id")?.as_str()?.trim();
    if uuid.is_empty() {
        return None;
    }
    let name = json
        .get("ps")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .or_else(|| fragment_name.as_deref())
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
    let network = json
        .get("net")
        .and_then(JsonValue::as_str)
        .map(|network| network.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "tcp".into());
    if network != "tcp" {
        fields.push(("network", yaml_str(&network)));
    }
    if network == "ws" {
        if let Some(opts) = websocket_opts(
            json.get("path").and_then(JsonValue::as_str),
            json.get("host").and_then(JsonValue::as_str),
        ) {
            fields.push(("ws-opts", opts));
        }
    } else if network == "h2" {
        if let Some(opts) = h2_opts(
            json.get("path").and_then(JsonValue::as_str),
            json.get("host").and_then(JsonValue::as_str),
        ) {
            fields.push(("h2-opts", opts));
        }
    } else if network == "http" {
        if let Some(opts) = http_opts(
            json.get("path").and_then(JsonValue::as_str),
            json.get("host").and_then(JsonValue::as_str),
        ) {
            fields.push(("http-opts", opts));
        }
    } else if network == "grpc" {
        if let Some(opts) = grpc_opts(
            json.get("serviceName")
                .and_then(JsonValue::as_str)
                .or_else(|| json.get("path").and_then(JsonValue::as_str)),
        ) {
            fields.push(("grpc-opts", opts));
        }
    }
    if vmess_tls_enabled(json.get("tls")) {
        fields.push(("tls", serde_yaml::Value::Bool(true)));
        if let Some(sni) = json
            .get("sni")
            .and_then(JsonValue::as_str)
            .filter(|sni| !sni.is_empty())
        {
            fields.push(("servername", yaml_str(sni)));
        }
        if json
            .get("tls")
            .and_then(JsonValue::as_str)
            .is_some_and(|tls| tls.trim().eq_ignore_ascii_case("reality"))
        {
            if let Some(opts) = reality_opts(
                json.get("pbk").and_then(JsonValue::as_str),
                json.get("sid").and_then(JsonValue::as_str),
            ) {
                fields.push(("reality-opts", opts));
            }
        }
    }
    if let Some(alpn) = json_csv(json.get("alpn")) {
        fields.push(("alpn", alpn));
    }
    if let Some(fingerprint) = json
        .get("fp")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        fields.push(("client-fingerprint", yaml_str(fingerprint)));
    }
    if json_bool(json.get("allowInsecure")) {
        fields.push(("skip-cert-verify", serde_yaml::Value::Bool(true)));
    }
    Some(mapping_node(&name, "vmess", &fields))
}

fn parse_vless(rest: &str) -> Option<ProxyNode> {
    let url = url::Url::parse(&format!("vless://{rest}")).ok()?;
    let uuid = percent_decode(url.username());
    if uuid.is_empty() {
        return None;
    }
    let server = url.host_str()?;
    let port = url.port().unwrap_or(443);
    if port == 0 {
        return None;
    }
    let name = url_name(&url).unwrap_or_else(|| server.to_string());
    let mut fields = vec![
        ("server", yaml_str(server)),
        ("port", yaml_int(port)),
        ("uuid", yaml_str(&uuid)),
        ("udp", serde_yaml::Value::Bool(true)),
    ];
    push_stream_fields(&mut fields, &url, "vless");
    Some(mapping_node(&name, "vless", &fields))
}

fn parse_trojan(rest: &str) -> Option<ProxyNode> {
    let url = url::Url::parse(&format!("trojan://{rest}")).ok()?;
    let password = urlencoding_username(&url)?;
    let server = url.host_str()?;
    let port = url.port().unwrap_or(443);
    if port == 0 {
        return None;
    }
    let name = url_name(&url).unwrap_or_else(|| server.to_string());
    let mut fields = vec![
        ("server", yaml_str(server)),
        ("port", yaml_int(port)),
        ("password", yaml_str(&password)),
        ("udp", serde_yaml::Value::Bool(true)),
    ];
    push_stream_fields(&mut fields, &url, "trojan");
    Some(mapping_node(&name, "trojan", &fields))
}

fn parse_hysteria2(rest: &str) -> Option<ProxyNode> {
    let url = url::Url::parse(&format!("hysteria2://{rest}")).ok()?;
    let server = url.host_str()?;
    let port = url.port().unwrap_or(443);
    if port == 0 {
        return None;
    }
    let name = url_name(&url).unwrap_or_else(|| server.to_string());
    let mut fields = vec![("server", yaml_str(server)), ("port", yaml_int(port))];
    if let Some(password) = urlencoding_username(&url) {
        if !password.is_empty() {
            fields.push(("password", yaml_str(&password)));
        }
    }
    if let Some(sni) = url_query(&url, "sni") {
        fields.push(("sni", yaml_str(&sni)));
    }
    if let Some(obfs) = url_query(&url, "obfs") {
        fields.push(("obfs", yaml_str(&obfs)));
    }
    if let Some(password) = url_query(&url, "obfs-password") {
        fields.push(("obfs-password", yaml_str(&password)));
    }
    if query_bool(
        url_query(&url, "insecure")
            .or_else(|| url_query(&url, "allow_insecure"))
            .as_deref(),
    ) {
        fields.push(("skip-cert-verify", serde_yaml::Value::Bool(true)));
    }
    if let Some(alpn) = yaml_csv(url_query(&url, "alpn").as_deref()) {
        fields.push(("alpn", alpn));
    }
    Some(mapping_node(&name, "hysteria2", &fields))
}

fn parse_anytls(rest: &str) -> Option<ProxyNode> {
    let url = url::Url::parse(&format!("anytls://{rest}")).ok()?;
    let password = urlencoding_username(&url)?;
    let server = url.host_str()?;
    let port = url.port().unwrap_or(443);
    if port == 0 {
        return None;
    }
    let name = url_name(&url).unwrap_or_else(|| server.to_string());
    let mut fields = vec![
        ("server", yaml_str(server)),
        ("port", yaml_int(port)),
        ("password", yaml_str(&password)),
        ("udp", serde_yaml::Value::Bool(true)),
    ];
    if let Some(sni) = url_query(&url, "sni").filter(|sni| !sni.is_empty()) {
        fields.push(("sni", yaml_str(&sni)));
    }
    if query_bool(
        url_query(&url, "insecure")
            .or_else(|| url_query(&url, "allow_insecure"))
            .as_deref(),
    ) {
        fields.push(("skip-cert-verify", serde_yaml::Value::Bool(true)));
    }
    if let Some(alpn) = yaml_csv(url_query(&url, "alpn").as_deref()) {
        fields.push(("alpn", alpn));
    }
    Some(mapping_node(&name, "anytls", &fields))
}

fn parse_tuic(rest: &str) -> Option<ProxyNode> {
    let url = url::Url::parse(&format!("tuic://{rest}")).ok()?;
    let uuid = percent_decode(url.username());
    let password = percent_decode(url.password()?);
    if uuid.is_empty() || password.is_empty() {
        return None;
    }
    let server = url.host_str()?;
    let port = url.port().unwrap_or(443);
    if port == 0 {
        return None;
    }
    let name = url_name(&url).unwrap_or_else(|| server.to_string());
    let mut fields = vec![
        ("server", yaml_str(server)),
        ("port", yaml_int(port)),
        ("uuid", yaml_str(&uuid)),
        ("password", yaml_str(&password)),
        ("udp", serde_yaml::Value::Bool(true)),
    ];
    if let Some(sni) = url_query(&url, "sni") {
        fields.push(("sni", yaml_str(&sni)));
    }
    if let Some(controller) = url_query(&url, "congestion_control") {
        fields.push(("congestion-controller", yaml_str(&controller)));
    }
    if let Some(mode) = url_query(&url, "udp_relay_mode") {
        fields.push(("udp-relay-mode", yaml_str(&mode)));
    }
    if query_bool(
        url_query(&url, "insecure")
            .or_else(|| url_query(&url, "allow_insecure"))
            .as_deref(),
    ) {
        fields.push(("skip-cert-verify", serde_yaml::Value::Bool(true)));
    }
    if let Some(alpn) = yaml_csv(url_query(&url, "alpn").as_deref()) {
        fields.push(("alpn", alpn));
    }
    Some(mapping_node(&name, "tuic", &fields))
}

fn push_stream_fields(fields: &mut Vec<(&str, serde_yaml::Value)>, url: &url::Url, kind: &str) {
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
    let network = get("type")
        .or_else(|| get("network"))
        .unwrap_or("tcp")
        .trim()
        .to_ascii_lowercase();
    if network != "tcp" {
        fields.push(("network", yaml_str(&network)));
    }
    if network == "ws" {
        if let Some(opts) = websocket_opts(get("path"), get("host")) {
            fields.push(("ws-opts", opts));
        }
    } else if network == "h2" {
        if let Some(opts) = h2_opts(get("path"), get("host")) {
            fields.push(("h2-opts", opts));
        }
    } else if network == "http" {
        if let Some(opts) = http_opts(get("path"), get("host")) {
            fields.push(("http-opts", opts));
        }
    } else if network == "grpc" {
        if let Some(opts) = grpc_opts(get("serviceName").or_else(|| get("path"))) {
            fields.push(("grpc-opts", opts));
        }
    }
    let security = get("security").unwrap_or("").to_ascii_lowercase();
    if security == "tls" || security == "reality" || query_bool(get("tls")) {
        fields.push(("tls", serde_yaml::Value::Bool(true)));
    }
    if let Some(sni) = get("sni")
        .or_else(|| get("serverName"))
        .filter(|sni| !sni.is_empty())
    {
        fields.push((
            if kind == "trojan" {
                "sni"
            } else {
                "servername"
            },
            yaml_str(sni),
        ));
    }
    if let Some(alpn) = yaml_csv(get("alpn")) {
        fields.push(("alpn", alpn));
    }
    if let Some(flow) = get("flow").filter(|flow| !flow.is_empty()) {
        fields.push(("flow", yaml_str(flow)));
    }
    if let Some(fingerprint) = get("fp").or_else(|| get("fingerprint")) {
        if !fingerprint.is_empty() {
            fields.push(("client-fingerprint", yaml_str(fingerprint)));
        }
    }
    if security == "reality" {
        if let Some(opts) = reality_opts(get("pbk"), get("sid")) {
            fields.push(("reality-opts", opts));
        }
    }
    if query_bool(
        get("allowInsecure")
            .or_else(|| get("allow_insecure"))
            .or_else(|| get("insecure")),
    ) {
        fields.push(("skip-cert-verify", serde_yaml::Value::Bool(true)));
    }
}

fn websocket_opts(path: Option<&str>, host: Option<&str>) -> Option<serde_yaml::Value> {
    let path = path.map(str::trim).filter(|path| !path.is_empty());
    let host = host.map(str::trim).filter(|host| !host.is_empty());
    if path.is_none() && host.is_none() {
        return None;
    }
    let mut opts = serde_yaml::Mapping::new();
    if let Some(path) = path {
        opts.insert(yaml_str("path"), yaml_str(path));
    }
    if let Some(host) = host {
        let mut headers = serde_yaml::Mapping::new();
        headers.insert(yaml_str("Host"), yaml_str(host));
        opts.insert(yaml_str("headers"), serde_yaml::Value::Mapping(headers));
    }
    Some(serde_yaml::Value::Mapping(opts))
}

fn url_query(url: &url::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

fn yaml_csv(value: Option<&str>) -> Option<serde_yaml::Value> {
    let values: Vec<serde_yaml::Value> = value
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(yaml_str)
        .collect();
    (!values.is_empty()).then_some(serde_yaml::Value::Sequence(values))
}

fn http_opts(path: Option<&str>, host: Option<&str>) -> Option<serde_yaml::Value> {
    let path = path.map(str::trim).filter(|path| !path.is_empty());
    let host = host.map(str::trim).filter(|host| !host.is_empty());
    if path.is_none() && host.is_none() {
        return None;
    }
    let mut opts = serde_yaml::Mapping::new();
    if let Some(path) = path {
        opts.insert(
            yaml_str("path"),
            serde_yaml::Value::Sequence(vec![yaml_str(path)]),
        );
    }
    if let Some(host) = host {
        let mut headers = serde_yaml::Mapping::new();
        headers.insert(
            yaml_str("Host"),
            serde_yaml::Value::Sequence(vec![yaml_str(host)]),
        );
        opts.insert(yaml_str("headers"), serde_yaml::Value::Mapping(headers));
    }
    Some(serde_yaml::Value::Mapping(opts))
}

fn h2_opts(path: Option<&str>, host: Option<&str>) -> Option<serde_yaml::Value> {
    let mut opts = serde_yaml::Mapping::new();
    if let Some(path) = path.filter(|path| !path.is_empty()) {
        opts.insert(yaml_str("path"), yaml_str(path));
    }
    if let Some(hosts) = yaml_csv(host) {
        opts.insert(yaml_str("host"), hosts);
    }
    (!opts.is_empty()).then_some(serde_yaml::Value::Mapping(opts))
}

fn grpc_opts(service_name: Option<&str>) -> Option<serde_yaml::Value> {
    let service_name = service_name
        .map(str::trim)
        .filter(|service_name| !service_name.is_empty())?;
    let mut opts = serde_yaml::Mapping::new();
    opts.insert(yaml_str("grpc-service-name"), yaml_str(service_name));
    Some(serde_yaml::Value::Mapping(opts))
}

fn reality_opts(public_key: Option<&str>, short_id: Option<&str>) -> Option<serde_yaml::Value> {
    let public_key = public_key.map(str::trim).filter(|value| !value.is_empty());
    let short_id = short_id.map(str::trim).filter(|value| !value.is_empty());
    if public_key.is_none() && short_id.is_none() {
        return None;
    }
    let mut opts = serde_yaml::Mapping::new();
    if let Some(public_key) = public_key {
        opts.insert(yaml_str("public-key"), yaml_str(public_key));
    }
    if let Some(short_id) = short_id {
        opts.insert(yaml_str("short-id"), yaml_str(short_id));
    }
    Some(serde_yaml::Value::Mapping(opts))
}

fn query_bool(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn vmess_tls_enabled(value: Option<&JsonValue>) -> bool {
    match value {
        Some(JsonValue::Bool(enabled)) => *enabled,
        Some(JsonValue::Number(value)) => value.as_u64().is_some_and(|value| value > 0),
        Some(JsonValue::String(value)) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "tls" | "xtls" | "reality"
        ),
        _ => false,
    }
}

fn json_bool(value: Option<&JsonValue>) -> bool {
    match value {
        Some(JsonValue::Bool(enabled)) => *enabled,
        Some(JsonValue::Number(value)) => value.as_u64().is_some_and(|value| value > 0),
        Some(JsonValue::String(value)) => query_bool(Some(value)),
        _ => false,
    }
}

fn json_csv(value: Option<&JsonValue>) -> Option<serde_yaml::Value> {
    match value {
        Some(JsonValue::String(value)) => yaml_csv(Some(value)),
        Some(JsonValue::Array(values)) => {
            let values: Vec<serde_yaml::Value> = values
                .iter()
                .filter_map(JsonValue::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(yaml_str)
                .collect();
            (!values.is_empty()).then_some(serde_yaml::Value::Sequence(values))
        }
        _ => None,
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
    (!host.is_empty() && port > 0).then_some((host, port))
}

fn split_uri_query(value: &str) -> (&str, Option<&str>) {
    if let Some((hostport, query)) = value.split_once("/?") {
        return (hostport, Some(query));
    }
    if let Some((hostport, query)) = value.split_once('?') {
        return (hostport.trim_end_matches('/'), Some(query));
    }
    (value.trim_end_matches('/'), None)
}

fn plugin_query<'a>(outer: Option<&'a str>, inline: Option<&'a str>) -> Option<&'a str> {
    [outer, inline].into_iter().flatten().find(|query| {
        query
            .split('&')
            .any(|pair| pair.split_once('=').is_some_and(|(key, _)| key == "plugin"))
    })
}

fn apply_shadowsocks_plugin(node: &mut ProxyNode, query: Option<&str>) {
    let Some(plugin) = query.and_then(|query| {
        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == "plugin").then(|| percent_decode(value))
        })
    }) else {
        return;
    };
    let mut parts = plugin.split(';');
    let Some(raw_name) = parts.next().map(str::trim).filter(|name| !name.is_empty()) else {
        return;
    };
    let plugin_name = match raw_name {
        "obfs-local" | "simple-obfs" => "obfs",
        name => name,
    };
    let Some(mapping) = node.spec.as_mapping_mut() else {
        return;
    };
    mapping.insert(yaml_str("plugin"), yaml_str(plugin_name));
    let mut options = serde_yaml::Mapping::new();
    if plugin_name == "v2ray-plugin" {
        options.insert(yaml_str("mode"), yaml_str("websocket"));
    }
    for item in parts {
        let (key, value) = item.split_once('=').unwrap_or((item, "true"));
        if key.is_empty() {
            continue;
        }
        let key = match key.trim() {
            "obfs" => "mode",
            "obfs-host" => "host",
            "obfs-uri" => "path",
            key => key,
        };
        let value = if matches!(key, "tls" | "mux" | "skip-cert-verify") {
            serde_yaml::Value::Bool(query_bool(Some(value)))
        } else {
            yaml_str(value)
        };
        options.insert(yaml_str(key), value);
    }
    if !options.is_empty() {
        mapping.insert(yaml_str("plugin-opts"), serde_yaml::Value::Mapping(options));
    }
}

fn url_name(url: &url::Url) -> Option<String> {
    let fragment = url.fragment()?.trim();
    (!fragment.is_empty()).then(|| percent_decode(fragment))
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

fn yaml_port(value: &serde_yaml::Value) -> Option<u16> {
    value
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .or_else(|| value.as_str().and_then(|port| port.parse().ok()))
}

fn decode_b64_payload(raw: &str) -> Option<Vec<u8>> {
    let payload = percent_decode(raw);
    b64(&payload).or_else(|| b64(payload.strip_suffix('/')?))
}

fn b64(raw: &str) -> Option<Vec<u8>> {
    let raw = raw.trim();
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(raw))
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(raw))
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(raw))
        .ok()
}

fn decode_text(raw: &str) -> Option<String> {
    let raw = raw
        .strip_prefix("base64://")
        .or_else(|| raw.strip_prefix("base64,"))
        .unwrap_or(raw);
    let compact: String = raw
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    if compact.len() < 16 {
        return None;
    }
    String::from_utf8(b64(&compact)?).ok()
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
    let client = crate::tls::http_client_builder()
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
    let client = crate::tls::http_client_builder()
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
    let client = crate::tls::http_client_builder()
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
#[path = "mihomo_subscription_tests.rs"]
mod subscription_tests;

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
    network: ws
    ws-opts:
      path: /chat
      headers:
        Host: cdn.example
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
        assert!(config.contains("ws-opts:"));
        assert!(config.contains("path: /chat"));
        assert!(config.contains("Host: cdn.example"));
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

        let wrapped = encoded
            .as_bytes()
            .chunks(20)
            .map(std::str::from_utf8)
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        let wrapped_nodes = parse_subscription(&wrapped).unwrap().nodes;
        assert_eq!(wrapped_nodes[0].name, "home");

        let plugin = parse_subscription(
            "ss://YWVzLTI1Ni1nY206cGFzcw@example.test:8388/?plugin=obfs-local%3Bobfs%3Dhttp%3Bobfs-host%3Dcdn.example#obfs",
        )
        .unwrap()
        .nodes;
        let plugin_text = serde_yaml::to_string(&plugin[0].spec).unwrap();
        assert!(plugin_text.contains("plugin: obfs"));
        assert!(plugin_text.contains("mode: http"));
        assert!(plugin_text.contains("host: cdn.example"));
    }

    #[test]
    fn parses_vmess_and_rejects_empty() {
        let payload = base64::engine::general_purpose::STANDARD.encode(
            br#"{"add":"vmess.example","port":"443","id":"11111111-1111-1111-1111-111111111111","aid":"0","net":"ws","tls":"tls","host":"cdn.example","path":"/chat","alpn":"h2,http/1.1","fp":"chrome","ps":"edge"}"#,
        );
        let nodes = parse_subscription(&format!("vmess://{payload}"))
            .unwrap()
            .nodes;
        assert_eq!(nodes[0].name, "edge");
        let text = serde_yaml::to_string(&nodes[0].spec).unwrap();
        assert!(text.contains("type: vmess"));
        assert!(text.contains("network: ws"));
        assert!(text.contains("ws-opts:"));
        assert!(text.contains("path: /chat"));
        assert!(text.contains("Host: cdn.example"));
        assert!(text.contains("- h2"));
        assert!(text.contains("client-fingerprint: chrome"));
        let config = render_config(&nodes_only(nodes), 17891, "127.0.0.1:17892", "secret");
        assert!(config.contains("ws-opts:"));
        assert!(config.contains("path: /chat"));
        assert!(config.contains("Host: cdn.example"));
        assert!(parse_subscription("   ").is_err());
        assert!(parse_subscription("not a subscription").is_err());
    }

    #[test]
    fn parses_websocket_options_from_vless_and_trojan_urls() {
        let vless = parse_subscription(
            "vless://uuid@example.test?type=ws&security=tls&sni=front.example&host=cdn.example&path=%2Fchat&alpn=h2%2Chttp%2F1.1#vless-ws",
        )
        .unwrap()
        .nodes;
        let vless_text = serde_yaml::to_string(&vless[0].spec).unwrap();
        assert!(vless_text.contains("network: ws"));
        assert!(vless_text.contains("tls: true"));
        assert!(vless_text.contains("servername: front.example"));
        assert!(vless_text.contains("path: /chat"));
        assert!(vless_text.contains("Host: cdn.example"));
        assert!(vless_text.contains("- h2"));

        let trojan = parse_subscription(
            "trojan://secret@example.test?type=ws&sni=front.example&host=cdn.example&path=%2Fchat&alpn=h2%2Chttp%2F1.1#trojan-ws",
        )
        .unwrap()
        .nodes;
        let trojan_text = serde_yaml::to_string(&trojan[0].spec).unwrap();
        assert!(trojan_text.contains("network: ws"));
        assert!(trojan_text.contains("sni: front.example"));
        assert!(trojan_text.contains("path: /chat"));
        assert!(trojan_text.contains("Host: cdn.example"));
        assert!(trojan_text.contains("- h2"));

        let reality = parse_subscription(
            "vless://uuid@example.test?type=grpc&serviceName=codex&security=reality&pbk=public-key&sid=short-id&fp=chrome",
        )
        .unwrap()
        .nodes;
        let reality_text = serde_yaml::to_string(&reality[0].spec).unwrap();
        assert!(reality_text.contains("network: grpc"));
        assert!(reality_text.contains("grpc-service-name: codex"));
        assert!(reality_text.contains("reality-opts:"));
        assert!(reality_text.contains("public-key: public-key"));
        assert!(reality_text.contains("short-id: short-id"));
        assert!(reality_text.contains("client-fingerprint: chrome"));
    }

    #[test]
    fn preserves_optional_transport_options_from_hysteria_and_tuic_urls() {
        let hysteria = parse_subscription(
            "hysteria2://secret@example.test:443?sni=front.example&obfs=salamander&obfs-password=obfs-secret&insecure=1&alpn=h3%2Ch3-29#hy2",
        )
        .unwrap()
        .nodes;
        let hysteria_text = serde_yaml::to_string(&hysteria[0].spec).unwrap();
        assert!(hysteria_text.contains("obfs: salamander"));
        assert!(hysteria_text.contains("obfs-password: obfs-secret"));
        assert!(hysteria_text.contains("skip-cert-verify: true"));
        assert!(hysteria_text.contains("- h3"));

        let tuic = parse_subscription(
            "tuic://uuid:p%40ss@example.test:443?sni=front.example&congestion_control=bbr&udp_relay_mode=native&allow_insecure=1#tuic",
        )
        .unwrap()
        .nodes;
        let tuic_text = serde_yaml::to_string(&tuic[0].spec).unwrap();
        assert!(tuic_text.contains("password: p@ss"));
        assert!(tuic_text.contains("congestion-controller: bbr"));
        assert!(tuic_text.contains("udp-relay-mode: native"));
        assert!(tuic_text.contains("skip-cert-verify: true"));
    }

    #[test]
    fn parses_anytls_share_links() {
        let nodes = parse_subscription(
            "anytls://secret@example.test:54101/?insecure=1&sni=front.example#Tokyo%20MPLS",
        )
        .unwrap()
        .nodes;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "Tokyo MPLS");
        let text = serde_yaml::to_string(&nodes[0].spec).unwrap();
        assert!(text.contains("type: anytls"));
        assert!(text.contains("password: secret"));
        assert!(text.contains("sni: front.example"));
        assert!(text.contains("skip-cert-verify: true"));
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
