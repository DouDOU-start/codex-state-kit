//! Codex (OpenAI) model prices and cost quotes.
//!
//! Prices follow sub2api: a LiteLLM-format catalog published at
//! `Wei-Shaw/model-price-repo`, refreshed when its `.sha256` changes, with a
//! bundled copy (`pricing_catalog.json`) as the offline fallback.  Only OpenAI
//! chat/responses models are kept.  The cost formula matches sub2api's
//! `computeTokenBreakdown`:
//!
//! ```text
//! uncached input = input - cache read - cache write
//! cost = uncached * input + cache read * cache_read + cache write * cache_write + output * output
//! ```
//!
//! Reasoning tokens are already part of `output_tokens` and are billed as
//! output.  Long-context (> threshold input) and service-tier prices are
//! applied the same way sub2api does.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

pub const REMOTE_URL: &str = "https://raw.githubusercontent.com/Wei-Shaw/model-price-repo/main/model_prices_and_context_window.json";
pub const HASH_URL: &str = "https://raw.githubusercontent.com/Wei-Shaw/model-price-repo/main/model_prices_and_context_window.sha256";
/// sub2api checks the remote hash every 10 minutes.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(10 * 60);

const BUNDLED: &str = include_str!("pricing_catalog.json");
const MAX_CATALOG_BYTES: usize = 16 * 1024 * 1024;
const PRIORITY_MULTIPLIER: f64 = 2.0;
const FLEX_MULTIPLIER: f64 = 0.5;
/// gpt-5.6 / gpt-6 charge 1.25x input for cache writes when no explicit price exists.
const CACHE_WRITE_PREMIUM: f64 = 1.25;
/// sub2api's last resort for unknown `gpt-*` names (`openai.DefaultTestModel`).
const FALLBACK_MODEL: &str = "gpt-5.4";
const EFFORT_SUFFIXES: [&str; 6] = ["none", "minimal", "low", "medium", "high", "xhigh"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    #[default]
    Standard,
    Priority,
    Flex,
}

impl ServiceTier {
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value?.trim().to_ascii_lowercase().as_str() {
            "priority" | "fast" => Some(Self::Priority),
            "flex" => Some(Self::Flex),
            "default" | "auto" | "standard" => Some(Self::Standard),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "standard",
            Self::Priority => "priority",
            Self::Flex => "flex",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Flex => 0,
            Self::Standard => 1,
            Self::Priority => 2,
        }
    }

    /// The tier to bill.  ChatGPT-account Codex responses report `default`
    /// even for fast mode, so `default` is not authoritative and the requested
    /// tier wins; any other reported tier may only lower the bill.
    pub fn billed(requested: Option<&str>, reported: Option<&str>) -> Self {
        let requested = Self::parse(requested).unwrap_or_default();
        let reported_default = reported
            .map(str::trim)
            .is_some_and(|value| value.eq_ignore_ascii_case("default"));
        match Self::parse(reported) {
            Some(reported) if !reported_default && reported.rank() < requested.rank() => reported,
            _ => requested,
        }
    }
}

/// USD per token for each usage bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rates {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl Rates {
    fn scaled(self, factor: f64) -> Self {
        Self {
            input: self.input * factor,
            cache_read: self.cache_read * factor,
            cache_write: self.cache_write * factor,
            output: self.output * factor,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LongContext {
    /// Applies when total input (including cached tokens) is above this.
    pub threshold: u64,
    pub input_multiplier: f64,
    pub output_multiplier: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPrice {
    pub standard: Rates,
    pub priority: Rates,
    pub flex: Rates,
    pub long_context: Option<LongContext>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// Total input, including cached reads and cache writes (OpenAI semantics).
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Includes reasoning tokens.
    pub output_tokens: u64,
}

/// Costs in nano-dollars (1e-9 USD).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostBreakdown {
    pub input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    pub total: i64,
    pub long_context: bool,
}

impl ModelPrice {
    pub fn rates(&self, tier: ServiceTier) -> Rates {
        match tier {
            ServiceTier::Standard => self.standard,
            ServiceTier::Priority => self.priority,
            ServiceTier::Flex => self.flex,
        }
    }

    pub fn quote(&self, usage: &Usage, tier: ServiceTier) -> CostBreakdown {
        let rates = self.rates(tier);
        let cache_read = usage.cache_read_tokens.min(usage.input_tokens);
        let cache_write = usage
            .cache_write_tokens
            .min(usage.input_tokens - cache_read);
        let uncached = usage.input_tokens - cache_read - cache_write;
        let long = self
            .long_context
            .filter(|lc| usage.input_tokens > lc.threshold);
        let (input_factor, output_factor) =
            long.map_or((1.0, 1.0), |lc| (lc.input_multiplier, lc.output_multiplier));
        let input = nanos(uncached, rates.input * input_factor);
        let cache_read = nanos(cache_read, rates.cache_read * input_factor);
        let cache_write = nanos(cache_write, rates.cache_write * input_factor);
        let output = nanos(usage.output_tokens, rates.output * output_factor);
        CostBreakdown {
            input,
            cache_read,
            cache_write,
            output,
            total: input + cache_read + cache_write + output,
            long_context: long.is_some(),
        }
    }
}

fn nanos(tokens: u64, usd_per_token: f64) -> i64 {
    (tokens as f64 * usd_per_token * 1e9).round() as i64
}

fn price_field(entry: &Map<String, Value>, key: &str) -> Option<f64> {
    entry
        .get(key)?
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// Strips a trailing `-YYYYMMDD` or `-YYYY-MM-DD` date.
fn strip_date(name: &str) -> Option<&str> {
    let bytes = name.as_bytes();
    let all_digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    let len = bytes.len();
    if len > 9 && bytes[len - 9] == b'-' && all_digits(len - 8..len) {
        return Some(&name[..len - 9]);
    }
    if len > 11
        && bytes[len - 11] == b'-'
        && bytes[len - 6] == b'-'
        && bytes[len - 3] == b'-'
        && all_digits(len - 10..len - 6)
        && all_digits(len - 5..len - 3)
        && all_digits(len - 2..len)
    {
        return Some(&name[..len - 11]);
    }
    None
}

fn strip_effort(name: &str) -> Option<&str> {
    let (base, suffix) = name.rsplit_once('-')?;
    EFFORT_SUFFIXES.contains(&suffix).then_some(base)
}

/// sub2api's `openAIModelFastPricingRatio`: these models' priority price is
/// always `standard x ratio`, whatever the catalog lists.
fn fast_ratio(key: &str) -> Option<f64> {
    match strip_date(key).unwrap_or(key) {
        "gpt-5.5" => Some(2.5),
        "gpt-5.4" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna" | "gpt-6-astra"
        | "gpt-6-sol" | "gpt-6-luna" => Some(2.0),
        _ => None,
    }
}

fn uses_cache_write_premium(key: &str) -> bool {
    key.starts_with("gpt-5.6") || key.starts_with("gpt-6")
}

fn long_context(entry: &Map<String, Value>, standard: &Rates) -> Option<LongContext> {
    let multiplier = |value: Option<f64>| value.filter(|m| *m > 0.0).unwrap_or(1.0);
    let context = if let Some(threshold) = entry
        .get("long_context_input_token_threshold")
        .and_then(Value::as_u64)
    {
        LongContext {
            threshold,
            input_multiplier: multiplier(price_field(entry, "long_context_input_cost_multiplier")),
            output_multiplier: multiplier(price_field(
                entry,
                "long_context_output_cost_multiplier",
            )),
        }
    } else {
        // `input_cost_per_token_above_272k_tokens` → 272k threshold with a
        // multiplier of above/base.  The smallest threshold wins.
        let thousands = entry
            .keys()
            .filter_map(|key| {
                key.strip_prefix("input_cost_per_token_above_")?
                    .strip_suffix("k_tokens")?
                    .parse::<u64>()
                    .ok()
            })
            .min()?;
        let ratio = |above: Option<f64>, base: f64| {
            multiplier(above.filter(|_| base > 0.0).map(|above| above / base))
        };
        LongContext {
            threshold: thousands * 1000,
            input_multiplier: ratio(
                price_field(
                    entry,
                    &format!("input_cost_per_token_above_{thousands}k_tokens"),
                ),
                standard.input,
            ),
            output_multiplier: ratio(
                price_field(
                    entry,
                    &format!("output_cost_per_token_above_{thousands}k_tokens"),
                ),
                standard.output,
            ),
        }
    };
    (context.input_multiplier > 1.0 || context.output_multiplier > 1.0).then_some(context)
}

fn parse_entry(key: &str, entry: &Map<String, Value>) -> Option<ModelPrice> {
    if entry.get("litellm_provider").and_then(Value::as_str) != Some("openai")
        || !matches!(
            entry.get("mode").and_then(Value::as_str),
            Some("chat" | "responses")
        )
    {
        return None;
    }
    let input = price_field(entry, "input_cost_per_token")?;
    let output = price_field(entry, "output_cost_per_token")?;
    // Without a separate price, cached reads and cache writes are plain input.
    let cache_read = price_field(entry, "cache_read_input_token_cost").unwrap_or(input);
    let cache_write = price_field(entry, "cache_creation_input_token_cost").unwrap_or(
        if uses_cache_write_premium(key) {
            input * CACHE_WRITE_PREMIUM
        } else {
            input
        },
    );
    let standard = Rates {
        input,
        cache_read,
        cache_write,
        output,
    };
    let tier = |suffix: &str, factor: f64| {
        let field = |name: &str, fallback: f64| {
            price_field(entry, &format!("{name}{suffix}")).unwrap_or(fallback * factor)
        };
        Rates {
            input: field("input_cost_per_token", standard.input),
            cache_read: field("cache_read_input_token_cost", standard.cache_read),
            cache_write: field("cache_creation_input_token_cost", standard.cache_write),
            output: field("output_cost_per_token", standard.output),
        }
    };
    let priority = match fast_ratio(key) {
        Some(ratio) => standard.scaled(ratio),
        None => tier("_priority", PRIORITY_MULTIPLIER),
    };
    Some(ModelPrice {
        standard,
        priority,
        flex: tier("_flex", FLEX_MULTIPLIER),
        long_context: long_context(entry, &standard),
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    Bundled,
    Cache,
    Remote,
}

pub struct Catalog {
    models: HashMap<String, ModelPrice>,
    source: CatalogSource,
    sha256: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Resolved<'a> {
    /// The catalog key whose price is used.
    pub key: &'a str,
    pub price: &'a ModelPrice,
}

impl Catalog {
    pub fn bundled() -> Self {
        Self::parse(BUNDLED.as_bytes(), CatalogSource::Bundled, None)
            .expect("bundled pricing catalog is valid")
    }

    /// Accepts both the upstream LiteLLM catalog and the trimmed
    /// `{ "source_sha256": ..., "models": { ... } }` form written by
    /// `tools/update-bundled-prices.mjs` and the local cache.
    fn parse(bytes: &[u8], source: CatalogSource, sha256: Option<String>) -> Result<Self> {
        let value: Value = serde_json::from_slice(bytes).context("价格表不是 JSON")?;
        let object = value.as_object().context("价格表不是 JSON 对象")?;
        let (entries, sha256) = match object.get("models").and_then(Value::as_object) {
            Some(models) => (
                models,
                sha256.or_else(|| {
                    object
                        .get("source_sha256")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                }),
            ),
            None => (object, sha256),
        };
        let models: HashMap<String, ModelPrice> = entries
            .iter()
            .filter_map(|(key, entry)| {
                let key = key.trim().to_ascii_lowercase();
                parse_entry(&key, entry.as_object()?).map(|price| (key, price))
            })
            .collect();
        anyhow::ensure!(
            models.contains_key(FALLBACK_MODEL),
            "价格表缺少 {FALLBACK_MODEL}"
        );
        Ok(Self {
            models,
            source,
            sha256: sha256.unwrap_or_default(),
        })
    }

    pub fn source(&self) -> CatalogSource {
        self.source
    }

    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn models(&self) -> impl Iterator<Item = (&str, &ModelPrice)> {
        self.models.iter().map(|(key, price)| (key.as_str(), price))
    }

    fn exact(&self, name: &str) -> Option<Resolved<'_>> {
        self.models
            .get_key_value(name)
            .map(|(key, price)| Resolved { key, price })
    }

    /// Maps a request model name to a catalog entry, in sub2api's order:
    /// canonical spelling, exact key, the name without reasoning-effort or date
    /// suffixes, the base version (`gpt-5.2-codex` → `gpt-5.2`), and finally
    /// `gpt-5.4` for any other `gpt-*` name.
    pub fn resolve(&self, model: &str) -> Option<Resolved<'_>> {
        let lower = model.trim().to_ascii_lowercase();
        let last = lower.rsplit('/').next().unwrap_or(&lower);
        let canonical = canonical_name(last);
        for candidate in [canonical.as_str(), lower.as_str(), last] {
            if let Some(found) = self.exact(candidate) {
                return Some(found);
            }
        }
        let mut name = canonical.as_str();
        while let Some(stripped) = strip_effort(name).or_else(|| strip_date(name)) {
            name = stripped;
            let aliased = canonical_name(name);
            if let Some(found) = self.exact(&aliased) {
                return Some(found);
            }
        }
        if name == "codex-mini-latest" || name.starts_with("gpt-5.3-codex") {
            return self.exact("gpt-5.3-codex");
        }
        let rest = name.strip_prefix("gpt-")?;
        let version_len = rest
            .find(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
            .unwrap_or(rest.len());
        let version = rest[..version_len].trim_end_matches('.');
        if !version.is_empty() {
            if let Some(found) = self.exact(&format!("gpt-{version}")) {
                return Some(found);
            }
        }
        self.exact(FALLBACK_MODEL)
    }
}

/// sub2api's `normalizeModelNameForPricing` for OpenAI names.
fn canonical_name(name: &str) -> String {
    let mut name = name.trim().trim_start_matches("models/").replace('_', "-");
    if let Some(rest) = name.strip_prefix("gpt") {
        if rest.starts_with(|ch: char| ch.is_ascii_digit()) {
            name = format!("gpt-{rest}");
        }
    }
    match name.as_str() {
        "gpt-6" => "gpt-6-astra".into(),
        "gpt-5.6" => "gpt-5.6-sol".into(),
        _ => match name.strip_prefix("gpt-5.6-") {
            Some(suffix)
                if suffix == "max"
                    || EFFORT_SUFFIXES.contains(&suffix)
                    || strip_date(&format!("x-{suffix}")) == Some("x") =>
            {
                "gpt-5.6-sol".into()
            }
            _ => name,
        },
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub last_checked_at: Option<String>,
    pub last_updated_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogInfo {
    pub source: CatalogSource,
    pub sha256: String,
    pub model_count: usize,
    pub remote_url: &'static str,
    #[serde(flatten)]
    pub sync: SyncStatus,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelPriceRow {
    pub model: String,
    #[serde(flatten)]
    pub price: ModelPrice,
}

/// The live catalog, swapped atomically when a newer remote copy arrives.
pub struct PriceBook {
    catalog: RwLock<Arc<Catalog>>,
    status: RwLock<SyncStatus>,
    cache_path: Option<PathBuf>,
}

impl PriceBook {
    /// The bundled catalog only; nothing is read from or written to disk.
    pub fn bundled() -> Self {
        Self {
            catalog: RwLock::new(Arc::new(Catalog::bundled())),
            status: RwLock::default(),
            cache_path: None,
        }
    }

    /// Loads the local cache when it is valid, otherwise the bundled catalog.
    pub fn load(cache_path: PathBuf) -> Self {
        let cached = std::fs::read(&cache_path)
            .ok()
            .and_then(|bytes| Catalog::parse(&bytes, CatalogSource::Cache, None).ok());
        Self {
            catalog: RwLock::new(Arc::new(cached.unwrap_or_else(Catalog::bundled))),
            status: RwLock::default(),
            cache_path: Some(cache_path),
        }
    }

    pub fn current(&self) -> Arc<Catalog> {
        self.catalog
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn info(&self) -> CatalogInfo {
        let catalog = self.current();
        CatalogInfo {
            source: catalog.source(),
            sha256: catalog.sha256().to_string(),
            model_count: catalog.len(),
            remote_url: REMOTE_URL,
            sync: self
                .status
                .read()
                .unwrap_or_else(|error| error.into_inner())
                .clone(),
        }
    }

    pub fn rows(&self) -> Vec<ModelPriceRow> {
        let catalog = self.current();
        let mut rows: Vec<ModelPriceRow> = catalog
            .models()
            .map(|(model, price)| ModelPriceRow {
                model: model.to_string(),
                price: price.clone(),
            })
            .collect();
        rows.sort_by(|a, b| a.model.cmp(&b.model));
        rows
    }

    /// Checks the remote hash and downloads the catalog when it changed.
    /// Unlike sub2api, a download whose sha256 does not match the published
    /// hash is rejected rather than only logged.  Returns whether prices changed.
    pub async fn sync(&self, client: &reqwest::Client) -> Result<bool> {
        let result = self.sync_inner(client).await;
        let now = chrono::Utc::now().to_rfc3339();
        let mut status = self
            .status
            .write()
            .unwrap_or_else(|error| error.into_inner());
        status.last_checked_at = Some(now.clone());
        match &result {
            Ok(changed) => {
                status.last_error = None;
                if *changed {
                    status.last_updated_at = Some(now);
                }
            }
            Err(error) => status.last_error = Some(format!("{error:#}")),
        }
        result
    }

    async fn sync_inner(&self, client: &reqwest::Client) -> Result<bool> {
        let remote_hash = fetch(client, HASH_URL).await?;
        let remote_hash = String::from_utf8_lossy(&remote_hash)
            .split_whitespace()
            .next()
            .map(str::to_ascii_lowercase)
            .filter(|hash| hash.len() == 64 && hash.chars().all(|ch| ch.is_ascii_hexdigit()))
            .context("远程价格哈希格式无效")?;
        if self.current().sha256().eq_ignore_ascii_case(&remote_hash) {
            return Ok(false);
        }
        let body = fetch(client, REMOTE_URL).await?;
        let actual = hex(&Sha256::digest(&body));
        anyhow::ensure!(
            actual == remote_hash,
            "价格表 sha256 不匹配（期望 {remote_hash}，实际 {actual}）"
        );
        let catalog = Catalog::parse(&body, CatalogSource::Remote, Some(remote_hash))?;
        if let Some(path) = &self.cache_path {
            if let Err(error) = write_cache(path, &body, catalog.sha256()) {
                eprintln!("[pricing] 写入价格缓存失败: {error:#}");
            }
        }
        *self
            .catalog
            .write()
            .unwrap_or_else(|error| error.into_inner()) = Arc::new(catalog);
        Ok(true)
    }
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let response = client
        .get(url)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("请求 {url} 失败"))?
        .error_for_status()
        .with_context(|| format!("请求 {url} 失败"))?;
    let bytes = response.bytes().await.context("读取价格数据失败")?;
    anyhow::ensure!(bytes.len() <= MAX_CATALOG_BYTES, "价格表过大");
    Ok(bytes.to_vec())
}

/// Caches only the OpenAI entries the billing code reads, in the same shape
/// as the bundled file, so the cache stays small.
fn write_cache(path: &std::path::Path, body: &[u8], sha256: &str) -> Result<()> {
    let value: Value = serde_json::from_slice(body)?;
    let models: Map<String, Value> = value
        .as_object()
        .context("价格表不是 JSON 对象")?
        .iter()
        .filter(|(key, entry)| {
            entry
                .as_object()
                .and_then(|entry| parse_entry(&key.to_ascii_lowercase(), entry))
                .is_some()
        })
        .map(|(key, entry)| (key.clone(), entry.clone()))
        .collect();
    let cache = serde_json::json!({ "source_sha256": sha256, "models": models });
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, serde_json::to_vec(&cache)?)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::bundled()
    }

    fn per_million(usd_per_token: f64) -> f64 {
        (usd_per_token * 1e6 * 1e6).round() / 1e6
    }

    #[test]
    fn bundled_catalog_has_codex_models() {
        let catalog = catalog();
        assert!(catalog.len() > 50);
        assert_eq!(catalog.sha256().len(), 64);
        let codex = catalog.resolve("gpt-5.1-codex").unwrap();
        assert_eq!(codex.key, "gpt-5.1-codex");
        assert_eq!(per_million(codex.price.standard.input), 1.25);
        assert_eq!(per_million(codex.price.standard.cache_read), 0.125);
        assert_eq!(per_million(codex.price.standard.output), 10.0);
        // No separate cache-write price: writes are billed as input.
        assert_eq!(per_million(codex.price.standard.cache_write), 1.25);
    }

    #[test]
    fn resolves_names_like_sub2api() {
        let catalog = catalog();
        let key = |name: &str| catalog.resolve(name).map(|found| found.key.to_string());
        assert_eq!(key("GPT-5.1-Codex").as_deref(), Some("gpt-5.1-codex"));
        assert_eq!(
            key("openai/gpt-5.1-codex").as_deref(),
            Some("gpt-5.1-codex")
        );
        assert_eq!(key("gpt5.2").as_deref(), Some("gpt-5.2"));
        assert_eq!(
            key("gpt-5.1-codex-mini-high").as_deref(),
            Some("gpt-5.1-codex-mini")
        );
        assert_eq!(
            key("gpt-5.2-codex-2026-01-15").as_deref(),
            Some("gpt-5.2-codex")
        );
        assert_eq!(key("gpt-5-codex-mini").as_deref(), Some("gpt-5"));
        assert_eq!(key("gpt-6").as_deref(), Some("gpt-6-astra"));
        assert_eq!(key("gpt-5.6").as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(key("gpt-5.6-xhigh").as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(
            key("gpt-5.3-codex-preview").as_deref(),
            Some("gpt-5.3-codex")
        );
        assert_eq!(key("codex-mini-latest").as_deref(), Some("gpt-5.3-codex"));
        assert_eq!(key("gpt-unknown").as_deref(), Some("gpt-5.4"));
        assert_eq!(key("claude-sonnet"), None);
    }

    #[test]
    fn cost_splits_input_cache_and_output() {
        let catalog = catalog();
        let price = catalog.resolve("gpt-5.1-codex").unwrap().price;
        let cost = price.quote(
            &Usage {
                input_tokens: 10_000,
                cache_read_tokens: 8_000,
                cache_write_tokens: 0,
                output_tokens: 1_000,
            },
            ServiceTier::Standard,
        );
        // 2000 x $1.25/M + 8000 x $0.125/M + 1000 x $10/M
        assert_eq!(cost.input, 2_500_000);
        assert_eq!(cost.cache_read, 1_000_000);
        assert_eq!(cost.output, 10_000_000);
        assert_eq!(cost.total, 13_500_000);
        assert!(!cost.long_context);
    }

    #[test]
    fn cache_writes_use_their_own_price() {
        let catalog = catalog();
        let price = catalog.resolve("gpt-6-astra").unwrap().price;
        assert_eq!(per_million(price.standard.cache_write), 12.5);
        let cost = price.quote(
            &Usage {
                input_tokens: 3_000,
                cache_read_tokens: 1_000,
                cache_write_tokens: 1_000,
                output_tokens: 0,
            },
            ServiceTier::Standard,
        );
        assert_eq!(cost.input, 10_000_000);
        assert_eq!(cost.cache_read, 1_000_000);
        assert_eq!(cost.cache_write, 12_500_000);
    }

    #[test]
    fn long_context_multiplies_input_and_output() {
        let catalog = catalog();
        let price = catalog.resolve("gpt-5.4").unwrap().price;
        let lc = price.long_context.unwrap();
        assert_eq!(lc.threshold, 272_000);
        assert_eq!(lc.input_multiplier, 2.0);
        assert_eq!(lc.output_multiplier, 1.5);
        let usage = |input_tokens| Usage {
            input_tokens,
            output_tokens: 1_000_000,
            ..Usage::default()
        };
        assert!(
            !price
                .quote(&usage(272_000), ServiceTier::Standard)
                .long_context
        );
        let long = price.quote(&usage(1_000_000), ServiceTier::Standard);
        assert!(long.long_context);
        assert_eq!(long.input, 5_000_000_000);
        assert_eq!(long.output, 22_500_000_000);
    }

    #[test]
    fn service_tiers_follow_sub2api() {
        let catalog = catalog();
        // gpt-5.5 priority is forced to 2.5x even though the catalog says 2x.
        let gpt55 = catalog.resolve("gpt-5.5").unwrap().price;
        assert_eq!(per_million(gpt55.priority.input), 12.5);
        assert_eq!(per_million(gpt55.priority.output), 75.0);
        // Catalog priority prices are used as-is for other models.
        let mini = catalog.resolve("gpt-5.1-codex-mini").unwrap().price;
        assert_eq!(per_million(mini.priority.input), 0.45);
        // Without catalog flex prices, flex is half of standard.
        assert_eq!(per_million(mini.flex.input), 0.125);

        assert_eq!(
            ServiceTier::billed(Some("priority"), Some("default")),
            ServiceTier::Priority
        );
        assert_eq!(
            ServiceTier::billed(Some("fast"), None),
            ServiceTier::Priority
        );
        assert_eq!(
            ServiceTier::billed(Some("priority"), Some("flex")),
            ServiceTier::Flex
        );
        assert_eq!(
            ServiceTier::billed(None, Some("priority")),
            ServiceTier::Standard
        );
        assert_eq!(ServiceTier::billed(None, None), ServiceTier::Standard);
    }

    #[test]
    fn parses_the_full_upstream_format() {
        let raw = serde_json::json!({
            "sample_spec": { "input_cost_per_token": 0 },
            "gpt-5.4": {
                "litellm_provider": "openai", "mode": "chat",
                "input_cost_per_token": 2.5e-6, "output_cost_per_token": 1.5e-5
            },
            "claude-x": {
                "litellm_provider": "anthropic", "mode": "chat",
                "input_cost_per_token": 1e-6, "output_cost_per_token": 1e-6
            },
            "gpt-image-2": {
                "litellm_provider": "openai", "mode": "image_generation",
                "input_cost_per_token": 1e-6, "output_cost_per_token": 1e-6
            }
        });
        let catalog = Catalog::parse(
            raw.to_string().as_bytes(),
            CatalogSource::Remote,
            Some("ab".repeat(32)),
        )
        .unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.sha256(), "ab".repeat(32));
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.json");
        let body = serde_json::json!({
            "gpt-5.4": {
                "litellm_provider": "openai", "mode": "chat",
                "input_cost_per_token": 3e-6, "output_cost_per_token": 1.5e-5
            },
            "claude-x": { "litellm_provider": "anthropic", "mode": "chat",
                "input_cost_per_token": 1e-6, "output_cost_per_token": 1e-6 }
        })
        .to_string();
        write_cache(&path, body.as_bytes(), &"cd".repeat(32)).unwrap();
        let book = PriceBook::load(path);
        let info = book.info();
        assert_eq!(info.source, CatalogSource::Cache);
        assert_eq!(info.sha256, "cd".repeat(32));
        assert_eq!(info.model_count, 1);
        let catalog = book.current();
        assert_eq!(
            per_million(catalog.resolve("gpt-5.4").unwrap().price.standard.input),
            3.0
        );
    }

    #[test]
    fn invalid_cache_falls_back_to_bundled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pricing.json");
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(PriceBook::load(path).info().source, CatalogSource::Bundled);
    }
}
