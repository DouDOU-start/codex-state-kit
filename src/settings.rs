use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use url::Url;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutboundMode {
    #[default]
    Manual,
    Mihomo,
}

fn deserialize_outbound_mode<'de, D>(deserializer: D) -> Result<OutboundMode, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref() {
        Some("mihomo") => Ok(OutboundMode::Mihomo),
        // `warp` is the removed built-in WARP line.
        Some("manual") | Some("warp") | None | Some("") => Ok(OutboundMode::Manual),
        Some(other) => Err(serde::de::Error::unknown_variant(
            other,
            &["manual", "mihomo"],
        )),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub proxy_listen: String,
    pub upstream: String,
    pub codex_home: String,
    pub outbound_proxy: String,
    #[serde(default, deserialize_with = "deserialize_outbound_mode")]
    pub outbound_mode: OutboundMode,
    pub forced_model: String,
    /// Clash / Mihomo 订阅 URL、本地文件，或分享链接正文。
    #[serde(default)]
    pub mihomo_subscription: String,
    /// 固定使用的节点名。空字符串表示连上后用订阅里的第一个。
    #[serde(default)]
    pub mihomo_node: String,
    /// Reach the manual proxy through the OS system proxy when one is set
    /// (Clash with only the system proxy on). See `system_proxy`.
    #[serde(default = "default_chain_system_proxy")]
    pub chain_system_proxy: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            proxy_listen: default_listen().into(),
            upstream: "https://chatgpt.com/backend-api/codex".into(),
            codex_home: home_dir().join(".codex").display().to_string(),
            outbound_proxy: String::new(),
            outbound_mode: OutboundMode::Manual,
            forced_model: String::new(),
            mihomo_subscription: String::new(),
            mihomo_node: String::new(),
            chain_system_proxy: default_chain_system_proxy(),
        }
    }
}

impl Settings {
    pub fn forced_model(&self) -> Option<&str> {
        let model = self.forced_model.trim();
        (!model.is_empty()).then_some(model)
    }
}

fn default_chain_system_proxy() -> bool {
    true
}

fn normalize_mihomo_text(raw: &str, max_len: usize, label: &str) -> Result<String> {
    let value = raw.trim();
    if value.len() > max_len {
        bail!("{label}过长");
    }
    Ok(value.to_string())
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
    #[serde(default, deserialize_with = "deserialize_outbound_mode")]
    pub outbound_mode: OutboundMode,
    #[serde(default)]
    pub forced_model: String,
    #[serde(default)]
    pub mihomo_subscription: String,
    #[serde(default)]
    pub mihomo_node: String,
    /// Reach the manual proxy through the OS system proxy when one is set
    /// (Clash with only the system proxy on). See `system_proxy`.
    #[serde(default = "default_chain_system_proxy")]
    pub chain_system_proxy: bool,
}

impl SettingsPatch {
    pub fn into_settings(self) -> Result<Settings> {
        let settings = Settings {
            proxy_listen: self.proxy_listen.trim().to_string(),
            upstream: self.upstream.trim().to_string(),
            codex_home: self.codex_home.trim().to_string(),
            outbound_proxy: normalize_outbound_proxy(&self.outbound_proxy)?,
            outbound_mode: self.outbound_mode,
            forced_model: normalize_forced_model(&self.forced_model)?,
            mihomo_subscription: normalize_mihomo_text(
                &self.mihomo_subscription,
                8192,
                "订阅地址",
            )?,
            mihomo_node: normalize_mihomo_text(&self.mihomo_node, 128, "节点名")?,
            chain_system_proxy: self.chain_system_proxy,
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
    // 旧的分路配置只剩一条线路：出站地址为空时，把原来的业务代理搬过来。
    if settings.outbound_proxy.trim().is_empty() {
        if let Some(upstream) = value
            .get("upstream_proxy")
            .and_then(serde_json::Value::as_str)
        {
            if let Ok(proxy) = normalize_outbound_proxy(upstream) {
                settings.outbound_proxy = proxy;
            }
        }
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
    normalize_model_id(raw, "强制绑定模型")
}

pub fn normalize_model_id(raw: &str, label: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(String::new());
    }
    if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("{label}不能包含空白或控制字符");
    }
    if raw.len() > 80 {
        bail!("{label}过长");
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
    let parsed = raw
        .replace("{session}", "sessionid")
        .replace("{SESSION}", "sessionid");
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
    fn settings_saved_with_turn_state_options_still_load() {
        let legacy = settings_from_json(
            r#"{"models":["a"],"state_miss_policy":"wait","token_reuse_policy":"shared_292","state_fetch_model":"a","token_fetch_paused":true,"token_max_age_mins":20,"token_prefetch_age_mins":10,"outbound_proxy":"http://127.0.0.1:7890","forced_model":"gpt-6-astra"}"#,
        )
        .unwrap();
        assert_eq!(legacy.outbound_proxy, "http://127.0.0.1:7890");
        assert_eq!(legacy.forced_model(), Some("gpt-6-astra"));
        let saved = serde_json::to_string(&legacy).unwrap();
        assert!(!saved.contains("state_miss_policy"));
        assert!(!saved.contains("token_max_age_mins"));
    }

    #[test]
    fn outbound_proxy_defaults_and_round_trips() {
        assert!(settings_from_json("{}").unwrap().outbound_proxy.is_empty());
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
                "codexHome": "test", "outboundProxy": format!(" {proxy} ")
            }))
            .unwrap();
            let settings = patch.into_settings().unwrap();
            let saved = settings_from_json(&serde_json::to_string(&settings).unwrap()).unwrap();
            assert_eq!(saved.outbound_proxy, proxy);
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
            proxy_listen: "127.0.0.1:8787".into(),
            upstream: "https://chatgpt.com/backend-api/codex".into(),
            codex_home: "/tmp/codex".into(),
            outbound_proxy: "socks5://127.0.0.1:1080".into(),
            outbound_mode: OutboundMode::Manual,
            forced_model: String::new(),
            mihomo_subscription: String::new(),
            mihomo_node: String::new(),
            chain_system_proxy: true,
        }
        .into_settings()
        .unwrap();
        assert_eq!(settings.outbound_proxy, "socks5://127.0.0.1:1080");
        assert_eq!(settings.outbound_mode, OutboundMode::Manual);
    }

    #[test]
    fn legacy_upstream_proxy_moves_into_the_single_outbound() {
        let kept = settings_from_json(
            r#"{"outbound_proxy":"socks5://localhost:1080","upstream_proxy":"http://127.0.0.1:7897"}"#,
        )
        .unwrap();
        assert_eq!(kept.outbound_proxy, "socks5://localhost:1080");
        let moved = settings_from_json(
            r#"{"upstream_proxy":"http://127.0.0.1:7897","network_route_policy":"separate"}"#,
        )
        .unwrap();
        assert_eq!(moved.outbound_proxy, "http://127.0.0.1:7897");
        assert_eq!(moved.outbound_mode, OutboundMode::Manual);
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
    fn manual_is_default_without_overriding_saved_choices() {
        assert_eq!(Settings::default().outbound_mode, OutboundMode::Manual);
        assert_eq!(
            settings_from_json("{}").unwrap().outbound_mode,
            OutboundMode::Manual
        );
        assert_eq!(
            settings_from_json(r#"{"outbound_mode":"warp"}"#)
                .unwrap()
                .outbound_mode,
            OutboundMode::Manual
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
    /// WARP was removed; settings saved with it load as the manual proxy.
    fn legacy_warp_mode_loads_as_manual_and_keeps_the_proxy() {
        let patch: SettingsPatch = serde_json::from_value(serde_json::json!({
            "proxyListen": "127.0.0.1:8787", "upstream": "https://example.com",
            "codexHome": "test", "outboundProxy": "socks5://localhost:1080", "outboundMode": "warp"
        }))
        .unwrap();
        let settings = patch.into_settings().unwrap();
        let saved: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(saved.outbound_mode, OutboundMode::Manual);
        assert_eq!(saved.outbound_proxy, "socks5://localhost:1080");
        assert!(serde_json::from_str::<OutboundMode>("\"unknown\"").is_err());
    }
}
