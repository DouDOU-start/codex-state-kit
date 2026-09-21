use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboundMode {
    #[default]
    Manual,
    Warp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StateMissPolicy {
    #[default]
    Preserve,
    Wait,
    Strip,
    Passthrough,
    #[serde(rename = "strip_all")]
    StripAll,
}

/// 仅同账号内共享精确 292 字节的 Turn-State，其他绑定长度保持模型隔离。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenReusePolicy {
    #[default]
    #[serde(rename = "shared_292")]
    Shared292,
    PerModel,
}

/// 业务转发与 Token 获取是否共用同一条出站线路。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkRoutePolicy {
    #[default]
    SameNetwork,
    Separate,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub proxy_listen: String,
    pub upstream: String,
    pub codex_home: String,
    pub outbound_proxy: String,
    pub upstream_proxy: String,
    #[serde(default)]
    pub outbound_mode: OutboundMode,
    pub warp_http2: bool,
    pub models: Vec<String>,
    pub state_miss_policy: StateMissPolicy,
    pub token_reuse_policy: TokenReusePolicy,
    pub network_route_policy: NetworkRoutePolicy,
    pub forced_model: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            proxy_listen: default_listen().into(),
            upstream: "https://chatgpt.com/backend-api/codex".into(),
            codex_home: home_dir().join(".codex").display().to_string(),
            outbound_proxy: String::new(),
            upstream_proxy: String::new(),
            outbound_mode: OutboundMode::Warp,
            warp_http2: false,
            models: vec![],
            state_miss_policy: StateMissPolicy::Preserve,
            token_reuse_policy: TokenReusePolicy::default(),
            network_route_policy: NetworkRoutePolicy::default(),
            forced_model: String::new(),
        }
    }
}

impl Settings {
    pub fn same_network(&self) -> bool {
        self.network_route_policy == NetworkRoutePolicy::SameNetwork
    }

    pub fn forced_model(&self) -> Option<&str> {
        let model = self.forced_model.trim();
        (!model.is_empty()).then_some(model)
    }
}

/// 开发环境用 8788，打包版用 8787，互不冲突
fn default_listen() -> &'static str {
    if is_dev_mode() {
        "127.0.0.1:8788"
    } else {
        "127.0.0.1:8787"
    }
}

/// debug 编译 = 开发模式
pub fn is_dev_mode() -> bool {
    cfg!(debug_assertions)
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPatch {
    pub proxy_listen: String,
    pub upstream: String,
    pub codex_home: String,
    #[serde(default)]
    pub outbound_proxy: String,
    #[serde(default)]
    pub upstream_proxy: String,
    #[serde(default)]
    pub outbound_mode: OutboundMode,
    #[serde(default)]
    pub warp_http2: bool,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub state_miss_policy: StateMissPolicy,
    #[serde(default)]
    pub token_reuse_policy: TokenReusePolicy,
    #[serde(default)]
    pub network_route_policy: NetworkRoutePolicy,
    #[serde(default)]
    pub forced_model: String,
}

impl SettingsPatch {
    pub fn into_settings(self) -> Result<Settings> {
        let models = self.models;
        let settings = Settings {
            proxy_listen: self.proxy_listen.trim().to_string(),
            upstream: self.upstream.trim().to_string(),
            codex_home: self.codex_home.trim().to_string(),
            outbound_proxy: normalize_outbound_proxy(&self.outbound_proxy)?,
            upstream_proxy: normalize_proxy(&self.upstream_proxy, "上游转发代理")?,
            outbound_mode: self.outbound_mode,
            warp_http2: self.warp_http2,
            models,
            state_miss_policy: self.state_miss_policy,
            token_reuse_policy: self.token_reuse_policy,
            network_route_policy: self.network_route_policy,
            forced_model: normalize_forced_model(&self.forced_model)?,
        };
        if settings.proxy_listen.is_empty()
            || settings.upstream.is_empty()
            || settings.codex_home.is_empty()
        {
            anyhow::bail!("listen / upstream / CODEX_HOME 不能为空");
        }
        let _: std::net::SocketAddr = settings.proxy_listen.parse().context("proxy_listen")?;
        Ok(settings)
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn settings_path() -> PathBuf {
    if is_dev_mode() {
        home_dir().join(".codex-state-kit-dev.json")
    } else {
        home_dir().join(".codex-state-kit.json")
    }
}

pub fn load_settings() -> Settings {
    let path = settings_path();
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| settings_from_json(&raw).ok())
        .unwrap_or_default()
}

fn settings_from_json(raw: &str) -> Result<Settings> {
    let value: serde_json::Value = serde_json::from_str(raw)?;
    let mut settings: Settings = serde_json::from_value(value.clone())?;
    // Preserve configured legacy proxies; use embedded WARP for unconfigured installs.
    if value.get("outbound_mode").is_none() && settings.outbound_proxy.trim().is_empty() {
        settings.outbound_mode = OutboundMode::Warp;
    }
    // 旧配置若已单独填写上游转发代理，保持分路，避免业务突然改走 Token 线路。
    if value.get("network_route_policy").is_none() && !settings.upstream_proxy.trim().is_empty() {
        settings.network_route_policy = NetworkRoutePolicy::Separate;
    }
    Ok(settings)
}

pub fn save_settings(settings: &Settings) -> Result<()> {
    let path = settings_path();
    let raw = serde_json::to_string_pretty(settings)?;
    std::fs::write(&path, raw).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

pub fn normalize_forced_model(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("强制绑定模型不能包含空白或控制字符");
    }
    if raw.len() > 80 {
        bail!("强制绑定模型过长");
    }
    Ok(raw.to_string())
}

pub fn normalize_outbound_proxy(raw: &str) -> Result<String> {
    normalize_proxy(raw, "出站代理")
}

pub fn normalize_proxy(raw: &str, label: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    let parsed = raw.replace("{session}", "sessionid").replace("{SESSION}", "sessionid");
    let url = Url::parse(&parsed).with_context(|| format!("{label}地址无效"))?;
    match url.scheme() {
        "http" | "https" | "socks5" | "socks5h" | "socks4" | "socks4a" => {}
        _ => bail!("不支持的{label}协议。请用 socks5:// 或 http://"),
    }
    if url.host_str().is_none() {
        bail!("{label}缺少主机");
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_policy_defaults_and_round_trips_without_losing_other_settings() {
        assert_eq!(settings_from_json("{}").unwrap().state_miss_policy, StateMissPolicy::Preserve);
        for (name, policy) in [
            ("preserve", StateMissPolicy::Preserve), ("wait", StateMissPolicy::Wait),
            ("strip", StateMissPolicy::Strip), ("passthrough", StateMissPolicy::Passthrough),
            ("strip_all", StateMissPolicy::StripAll),
        ] {
            let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
                "proxyListen":"127.0.0.1:8787", "upstream":"https://example.com", "codexHome":"test",
                "stateMissPolicy":name, "models":["test-model"], "upstreamProxy":"http://127.0.0.1:7890"
            })).unwrap();
            let settings = patch.into_settings().unwrap();
            let loaded = settings_from_json(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(loaded.state_miss_policy, policy);
            assert_eq!(loaded.models, ["test-model"]);
            assert_eq!(loaded.upstream_proxy, "http://127.0.0.1:7890");
        }
        assert!(serde_json::from_value::<StateMissPolicy>(serde_json::json!("unknown")).is_err());
    }

    #[test]
    fn token_reuse_policy_defaults_and_round_trips() {
        assert_eq!(Settings::default().token_reuse_policy, TokenReusePolicy::Shared292);
        let legacy = settings_from_json(r#"{"models":["a","b"],"outbound_mode":"manual"}"#).unwrap();
        assert_eq!(legacy.token_reuse_policy, TokenReusePolicy::Shared292);
        assert_eq!(legacy.models, ["a", "b"]);
        for (name, policy) in [("shared_292", TokenReusePolicy::Shared292), ("per_model", TokenReusePolicy::PerModel)] {
            let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
                "proxyListen":"127.0.0.1:8787", "upstream":"https://example.com", "codexHome":"test",
                "tokenReusePolicy":name, "stateMissPolicy":"wait", "models":["a","b"]
            })).unwrap();
            let settings = patch.into_settings().unwrap();
            let loaded = settings_from_json(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(loaded.token_reuse_policy, policy);
            assert_eq!(loaded.state_miss_policy, StateMissPolicy::Wait);
            assert_eq!(loaded.models, ["a", "b"]);
        }
        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "proxyListen":"127.0.0.1:8787", "upstream":"https://example.com", "codexHome":"test"
        })).unwrap();
        assert_eq!(patch.token_reuse_policy, TokenReusePolicy::Shared292);
        assert!(serde_json::from_str::<TokenReusePolicy>("\"unknown\"").is_err());
    }

    #[test]
    fn upstream_proxy_defaults_and_round_trips() {
        assert!(settings_from_json("{}").unwrap().upstream_proxy.is_empty());
        for proxy in [
            "",
            "http://127.0.0.1:7897",
            "https://localhost:7897",
            "socks5://localhost:1080",
            "socks5h://localhost:1080",
            "socks4://localhost:1080",
            "socks4a://localhost:1080",
        ] {
            let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
                "proxyListen": "127.0.0.1:8787", "upstream": "https://example.com",
                "codexHome": "test", "upstreamProxy": format!(" {proxy} ")
            }))
            .unwrap();
            let settings = patch.into_settings().unwrap();
            let saved = settings_from_json(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(saved.upstream_proxy, proxy);
        }
        for proxy in [
            "not a url",
            "ftp://user:secret@localhost:21",
            "http://localhost:99999",
        ] {
            let err = normalize_proxy(proxy, "上游转发代理").unwrap_err();
            let message = format!("{err:#}");
            assert!(message.contains("上游转发代理"));
            assert!(!message.contains("secret"));
        }
    }

    #[test]
    fn empty_outbound_proxy_is_ok() {
        assert_eq!(normalize_outbound_proxy("  ").unwrap(), "");
    }

    #[test]
    fn accepts_session_placeholder_in_proxy_url() {
        let raw = "socks5://xmtt1126849-region-DE-sid-{session}-t-120:pass@us.arxlabs.io:3010";
        assert_eq!(normalize_outbound_proxy(raw).unwrap(), raw);
        assert_eq!(normalize_proxy(raw, "上游转发代理").unwrap(), raw);
    }

    #[test]
    fn accepts_socks_and_http() {
        assert_eq!(
            normalize_outbound_proxy("socks5://127.0.0.1:1080").unwrap(),
            "socks5://127.0.0.1:1080"
        );
        assert_eq!(
            normalize_outbound_proxy("http://127.0.0.1:7890").unwrap(),
            "http://127.0.0.1:7890"
        );
    }

    #[test]
    fn rejects_unknown_scheme() {
        let err = normalize_outbound_proxy("ftp://127.0.0.1:21").unwrap_err();
        assert!(err.to_string().contains("不支持的出站代理协议"));
    }

    #[test]
    fn patch_keeps_optional_proxy() {
        let settings = SettingsPatch {
            token_reuse_policy: TokenReusePolicy::default(),
            state_miss_policy: StateMissPolicy::Preserve,
            proxy_listen: "127.0.0.1:8787".into(),
            upstream: "https://chatgpt.com/backend-api/codex".into(),
            codex_home: "/tmp/codex".into(),
            outbound_proxy: "socks5://127.0.0.1:1080".into(),
            upstream_proxy: String::new(),
            outbound_mode: OutboundMode::Manual,
            warp_http2: false,
            models: vec![],
            network_route_policy: NetworkRoutePolicy::SameNetwork,
            forced_model: String::new(),
        }
        .into_settings()
        .unwrap();
        assert_eq!(settings.outbound_proxy, "socks5://127.0.0.1:1080");
        assert_eq!(settings.network_route_policy, NetworkRoutePolicy::SameNetwork);
    }

    #[test]
    fn network_route_policy_defaults_and_legacy_upstream_stays_separate() {
        assert_eq!(
            Settings::default().network_route_policy,
            NetworkRoutePolicy::SameNetwork
        );
        assert!(settings_from_json("{}").unwrap().same_network());
        let legacy_split = settings_from_json(
            r#"{"outbound_proxy":"socks5://localhost:1080","upstream_proxy":"http://127.0.0.1:7897"}"#,
        )
        .unwrap();
        assert_eq!(legacy_split.network_route_policy, NetworkRoutePolicy::Separate);
        let explicit = settings_from_json(
            r#"{"upstream_proxy":"http://127.0.0.1:7897","network_route_policy":"same_network"}"#,
        )
        .unwrap();
        assert!(explicit.same_network());
        for (name, policy) in [
            ("same_network", NetworkRoutePolicy::SameNetwork),
            ("separate", NetworkRoutePolicy::Separate),
        ] {
            let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
                "proxyListen":"127.0.0.1:8787", "upstream":"https://example.com", "codexHome":"test",
                "networkRoutePolicy":name
            }))
            .unwrap();
            let settings = patch.into_settings().unwrap();
            let loaded = settings_from_json(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(loaded.network_route_policy, policy);
        }
        assert!(serde_json::from_str::<NetworkRoutePolicy>("\"unknown\"").is_err());
    }

    #[test]
    fn forced_model_defaults_and_round_trips() {
        assert!(Settings::default().forced_model().is_none());
        assert!(settings_from_json("{}").unwrap().forced_model().is_none());
        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "proxyListen":"127.0.0.1:8787", "upstream":"https://example.com", "codexHome":"test",
            "forcedModel":" gpt-6-astra "
        }))
        .unwrap();
        let settings = patch.into_settings().unwrap();
        assert_eq!(settings.forced_model(), Some("gpt-6-astra"));
        let loaded = settings_from_json(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(loaded.forced_model(), Some("gpt-6-astra"));
        assert!(normalize_forced_model("gpt 6").is_err());
        assert!(normalize_forced_model(&"m".repeat(81)).is_err());
    }

    #[test]
    fn legacy_settings_keep_manual_proxy() {
        let settings: Settings =
            serde_json::from_str(r#"{"outbound_proxy":"http://localhost:7890"}"#).unwrap();
        assert_eq!(settings.outbound_mode, OutboundMode::Manual);
        assert_eq!(settings.outbound_proxy, "http://localhost:7890");
    }

    #[test]
    fn embedded_warp_is_default_without_overriding_saved_choices() {
        assert_eq!(Settings::default().outbound_mode, OutboundMode::Warp);
        assert_eq!(
            settings_from_json("{}").unwrap().outbound_mode,
            OutboundMode::Warp
        );
        assert_eq!(
            settings_from_json(r#"{"outbound_proxy":"http://localhost:7890"}"#)
                .unwrap()
                .outbound_mode,
            OutboundMode::Manual
        );
        assert_eq!(
            settings_from_json(r#"{"outbound_mode":"manual"}"#)
                .unwrap()
                .outbound_mode,
            OutboundMode::Manual
        );
    }

    #[test]
    fn warp_selection_preserves_manual_url_and_round_trips() {
        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "proxyListen": "127.0.0.1:8787", "upstream": "https://example.com",
            "codexHome": "test", "outboundProxy": "socks5://localhost:1080", "outboundMode": "warp"
        }))
        .unwrap();
        let settings = patch.into_settings().unwrap();
        let saved: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(saved.outbound_mode, OutboundMode::Warp);
        assert_eq!(saved.outbound_proxy, "socks5://localhost:1080");
        assert!(serde_json::from_str::<OutboundMode>("\"unknown\"").is_err());
    }
}
