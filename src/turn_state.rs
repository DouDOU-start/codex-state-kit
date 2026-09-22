use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use chrono::{SecondsFormat, Utc};
use http::{HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::chatgpt_cookies::{self, RoutingCookie};
use crate::settings::TokenReusePolicy;

pub const HEADER_NAME: &str = "x-codex-turn-state";
/// 凭据包新鲜窗口：240 秒后不再注入。
pub const MAX_AGE_SECS: i64 = 240;
/// 采到 292 后暂停这么久再采下一张，持续刷新凭据包。
pub const PREFETCH_AGE_SECS: i64 = 30;
/// 与探针校验一致，允许小幅服务端时钟偏移。
pub(crate) const MAX_FUTURE_SKEW_SECS: i64 = 300;
/// 模型活跃窗口：60 分钟内有请求则视为活跃，持续预取
const ACTIVE_WINDOW_SECS: i64 = 3600;

/// 正常 token 长度约 292。一个账号只会出 292 或 332，不会同时出现。
pub const QUALITY_TOKEN_LEN: usize = 292;
/// 另一档正常 token 长度约 332
pub const QUALITY_TOKEN_LEN_332: usize = 332;
/// 降智 token 长度约 312
pub const DEGRADED_TOKEN_LEN: usize = 312;

/// 每个模型在池中最多保留多少种不同长度的 token
const MAX_POOL_VARIANTS_PER_MODEL: usize = 10;
/// 池中 token 最大保留时长（2 小时，超过就清除）
const POOL_MAX_AGE_SECS: i64 = 7200;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TurnState {
    pub token: String,
    pub issued_unix: i64,
    pub len: usize,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub captured_at: String,
    /// 打到该票据时使用的代理 session；业务转发复用同一出口。
    #[serde(default)]
    pub proxy_session: String,
    /// 探针第一包返回的 response.id，用来把下游请求包装成同轮第二包。
    #[serde(default)]
    pub previous_response_id: String,
    /// 打到该票据时上游下发的 Cloudflare / 线路 cookie；业务转发回放到同一边缘。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routing_cookies: Vec<RoutingCookie>,
}

/// 单种 token 长度的计数
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenLenCount {
    pub len: usize,
    pub count: u32,
}

/// 池中单个缓存 token 的摘要
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolTokenInfo {
    pub len: usize,
    pub age_secs: i64,
    /// 是否匹配当前绑定长度
    pub is_bound: bool,
    /// 是否仍在有效期内（≤40min）
    pub is_valid: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelTokenView {
    pub model: String,
    pub status: String,
    pub age_secs: Option<i64>,
    pub len: Option<usize>,
    pub captured_at: Option<String>,
    /// 最近一轮 fetch 的 token 长度分布（各长度出现次数）
    pub distribution: Vec<TokenLenCount>,
    /// 池中实际缓存的各长度 token 信息
    pub pool_tokens: Vec<PoolTokenInfo>,
    /// 模型级绑定覆盖（None 表示跟随全局）
    pub bound_override: Option<usize>,
    /// 共享模式实际使用的票据来源模型；不伪造目标模型的缓存或获取记录。
    pub shared_from_model: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnStateView {
    pub status: String,
    pub age_secs: Option<i64>,
    pub len: Option<usize>,
    pub source: Option<String>,
    pub captured_at: Option<String>,
    pub models: Vec<ModelTokenView>,
    /// 当前绑定的 token 长度（用户指定，或账号自动识别的 292/332）
    pub bound_token_len: usize,
    pub shared_source_model: Option<String>,
    pub bound_proxy_session: Option<String>,
}

// ─── 持久化结构 ───────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct PersistedStore {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    tokens: HashMap<String, TurnState>,
    /// model → 上次从代理请求中见到的 unix 时间戳
    #[serde(default)]
    active_models: HashMap<String, i64>,
    /// 全局绑定的 token 长度，None = 跟随账号自动识别
    #[serde(default)]
    bound_token_len: Option<usize>,
    /// 该账号见到的质量 token 长度（292 或 332）
    #[serde(default)]
    auto_quality_len: Option<usize>,
    /// 模型级绑定覆盖：model → 该模型专属的绑定长度
    #[serde(default)]
    model_bound_lens: HashMap<String, usize>,
    /// 所有长度的 token 缓存池：model → [各长度的 TurnState]
    #[serde(default)]
    pool: HashMap<String, Vec<TurnState>>,
    /// 每个模型最近一轮 fetch 的 token 长度分布
    #[serde(default)]
    distributions: HashMap<String, Vec<TokenLenCount>>,
    /// 当前 Token 池所属的 ChatGPT account_id
    #[serde(default)]
    account_id: Option<String>,
}

const PERSIST_VERSION: u32 = 2;

// ─── TurnStateStore ───────────────────────────────────────────

/// 按来源模型保存票据；读取和预取按配置选择共享 292 或模型隔离。
pub struct TurnStateStore {
    /// 来自应用设置，不写入票据文件，切换策略不复制或清空原模型缓存。
    reuse_policy: TokenReusePolicy,
    /// 当前绑定长度的活跃 token（直接用于注入）
    tokens: HashMap<String, TurnState>,
    /// 所有长度的 token 缓存：model → [各长度最新 TurnState]
    pool: HashMap<String, Vec<TurnState>>,
    /// model → 上次请求经过代理时的 unix 时间戳
    active_models: HashMap<String, i64>,
    /// 全局绑定的 token 长度（None = 跟随账号自动识别的 292/332）
    bound_token_len: Option<usize>,
    /// 该账号见到的质量 token 长度（292 或 332）
    auto_quality_len: Option<usize>,
    /// 模型级绑定覆盖：model → 该模型专属的绑定长度（优先于全局）
    model_bound_lens: HashMap<String, usize>,
    /// 每个模型最近一轮 fetch 的 token 长度分布
    distributions: HashMap<String, Vec<TokenLenCount>>,
    /// 当前 Token 池所属的 ChatGPT account_id
    account_id: Option<String>,
    /// 来自应用设置，不写入票据文件。
    max_age_secs: i64,
    prefetch_age_secs: i64,
}

impl Default for TurnStateStore {
    fn default() -> Self {
        Self {
            reuse_policy: TokenReusePolicy::default(),
            tokens: HashMap::new(),
            pool: HashMap::new(),
            active_models: HashMap::new(),
            bound_token_len: None,
            auto_quality_len: None,
            model_bound_lens: HashMap::new(),
            distributions: HashMap::new(),
            account_id: None,
            max_age_secs: MAX_AGE_SECS,
            prefetch_age_secs: PREFETCH_AGE_SECS,
        }
    }
}

fn token_path() -> std::path::PathBuf {
    use crate::settings::{home_dir, is_dev_mode};
    if is_dev_mode() {
        home_dir().join(".codex-state-kit-dev-token.json")
    } else {
        home_dir().join(".codex-state-kit-token.json")
    }
}

impl TurnStateStore {
    pub fn load() -> Self {
        let raw = match std::fs::read_to_string(token_path()) {
            Ok(raw) => raw,
            Err(_) => return Self::default(),
        };

        // v2 格式
        if let Ok(store) = serde_json::from_str::<PersistedStore>(&raw) {
            if store.version >= PERSIST_VERSION {
                let bound = store.bound_token_len;
                let tokens: HashMap<String, TurnState> = store
                    .tokens
                    .into_iter()
                    .filter(|(_, ts)| ts.token.starts_with("gAAAAA"))
                    .collect();
                // 恢复池——过滤无效 token
                let pool: HashMap<String, Vec<TurnState>> = store
                    .pool
                    .into_iter()
                    .map(|(model, entries)| {
                        let valid: Vec<TurnState> = entries
                            .into_iter()
                            .filter(|ts| ts.token.starts_with("gAAAAA"))
                            .collect();
                        (model, valid)
                    })
                    .filter(|(_, v)| !v.is_empty())
                    .collect();
                for (model, ts) in &tokens {
                    eprintln!(
                        "[token] 从磁盘恢复模型 {} 的 token（{}字节，age={}s）",
                        model, ts.len, now_unix() - ts.issued_unix
                    );
                }
                let pool_count: usize = pool.values().map(|v| v.len()).sum();
                if pool_count > 0 {
                    eprintln!("[token] 恢复 {} 个缓存池 token", pool_count);
                }
                let active_count = store.active_models.len();
                if active_count > 0 {
                    eprintln!("[token] 恢复 {} 个活跃模型追踪", active_count);
                }
                if let Some(bl) = bound {
                    eprintln!("[token] 恢复绑定 token 长度: {}", bl);
                }
                let mbl = store.model_bound_lens;
                if !mbl.is_empty() {
                    eprintln!("[token] 恢复 {} 个模型级绑定", mbl.len());
                }
                // 兼容：旧版本没有 pool，把 tokens 补入 pool
                let mut pool = pool;
                for (model, ts) in &tokens {
                    let entries = pool.entry(model.clone()).or_default();
                    if !entries.iter().any(|e| is_matching_token_len(e.len, ts.len)) {
                        entries.push(ts.clone());
                        eprintln!("[token] 将 {} 的 token（{}字节）补入缓存池", model, ts.len);
                    }
                }
                let dist = store.distributions;
                if !dist.is_empty() {
                    eprintln!("[token] 恢复 {} 个模型的分布数据", dist.len());
                }
                let auto = store
                    .auto_quality_len
                    .and_then(canonical_quality_len)
                    .or_else(|| infer_auto_quality_len(&tokens, &pool));
                if let Some(len) = auto {
                    eprintln!("[token] 账号默认质量长度: {}", len);
                }
                return Self {
                    reuse_policy: TokenReusePolicy::default(),
                    tokens,
                    pool,
                    active_models: store.active_models,
                    bound_token_len: bound,
                    auto_quality_len: auto,
                    model_bound_lens: mbl,
                    distributions: dist,
                    account_id: store.account_id,
                    max_age_secs: MAX_AGE_SECS,
                    prefetch_age_secs: PREFETCH_AGE_SECS,
                };
            }
        }

        // v1 格式
        if let Ok(map) = serde_json::from_str::<HashMap<String, TurnState>>(&raw) {
            let tokens: HashMap<String, TurnState> = map
                .into_iter()
                .filter(|(_, ts)| ts.token.starts_with("gAAAAA"))
                .collect();
            if !tokens.is_empty() {
                let now = now_unix();
                let active_models: HashMap<String, i64> =
                    tokens.keys().map(|k| (k.clone(), now)).collect();
                for (model, ts) in &tokens {
                    eprintln!(
                        "[token] 从磁盘恢复 v1 格式模型 {} 的 token（{}字节），自动注册追踪",
                        model, ts.len
                    );
                }
                // tokens 同步复制到 pool
                let pool: HashMap<String, Vec<TurnState>> = tokens
                    .iter()
                    .map(|(model, ts)| (model.clone(), vec![ts.clone()]))
                    .collect();
                let auto = infer_auto_quality_len(&tokens, &pool);
                return Self {
                    reuse_policy: TokenReusePolicy::default(),
                    tokens,
                    pool,
                    active_models,
                    bound_token_len: None,
                    auto_quality_len: auto,
                    model_bound_lens: HashMap::new(),
                    distributions: HashMap::new(),
                    account_id: None,
                    max_age_secs: MAX_AGE_SECS,
                    prefetch_age_secs: PREFETCH_AGE_SECS,
                };
            }
        }

        // 旧格式：单个 TurnState
        if let Ok(ts) = serde_json::from_str::<TurnState>(&raw) {
            if ts.token.starts_with("gAAAAA") {
                eprintln!("[token] 从磁盘恢复旧格式 token（{}字节），放入 _default", ts.len);
                let auto = canonical_quality_len(ts.len);
                let mut tokens = HashMap::new();
                tokens.insert("_default".to_string(), ts.clone());
                let mut pool = HashMap::new();
                pool.insert("_default".to_string(), vec![ts]);
                return Self {
                    reuse_policy: TokenReusePolicy::default(),
                    tokens,
                    pool,
                    active_models: HashMap::new(),
                    bound_token_len: None,
                    auto_quality_len: auto,
                    model_bound_lens: HashMap::new(),
                    distributions: HashMap::new(),
                    account_id: None,
                    max_age_secs: MAX_AGE_SECS,
                    prefetch_age_secs: PREFETCH_AGE_SECS,
                };
            }
        }

        Self::default()
    }

    fn persist(&mut self) {
        // 持久化前先清理过期/多余 token
        self.cleanup_pool();
        let path = token_path();
        let store = PersistedStore {
            version: PERSIST_VERSION,
            tokens: self.tokens.clone(),
            active_models: self.active_models.clone(),
            bound_token_len: self.bound_token_len,
            auto_quality_len: self.auto_quality_len,
            model_bound_lens: self.model_bound_lens.clone(),
            pool: self.pool.clone(),
            distributions: self.distributions.clone(),
            account_id: self.account_id.clone(),
        };
        if let Ok(raw) = serde_json::to_string_pretty(&store) {
            let _ = std::fs::write(&path, raw);
        }
    }

    // ─── 模型自动发现 ──────────────────────────────────────────

    /// 从代理请求中发现模型，更新 last_seen 时间戳。
    /// 返回 true 表示这是一个全新发现的模型。
    pub fn register_model(&mut self, model: &str) -> bool {
        let is_new = !self.active_models.contains_key(model);
        self.active_models.insert(model.to_string(), now_unix());
        if is_new {
            self.persist();
        }
        is_new
    }

    /// 返回最近 60 分钟内有请求经过的活跃模型列表。
    pub fn all_active_models(&self) -> Vec<String> {
        let cutoff = now_unix() - ACTIVE_WINDOW_SECS;
        let mut models: Vec<String> = self
            .active_models
            .iter()
            .filter(|(_, &last_seen)| last_seen > cutoff)
            .map(|(model, _)| model.clone())
            .collect();
        models.sort();
        models
    }

    // ─── 绑定 / 分布 ───────────────────────────────────────────

    /// 全局绑定的目标 token 长度。未手动指定时跟随账号见到的 292 或 332。
    pub fn bound_len(&self) -> usize {
        self.bound_token_len
            .or(self.auto_quality_len)
            .unwrap_or(QUALITY_TOKEN_LEN)
    }

    /// 获取某模型实际使用的绑定长度（模型级优先，否则用全局）
    pub fn bound_len_for(&self, model: &str) -> usize {
        self.model_bound_lens
            .get(model)
            .copied()
            .unwrap_or_else(|| self.bound_len())
    }

    /// 获取模型级绑定信息（None 表示跟随全局）
    pub fn model_bound_len(&self, model: &str) -> Option<usize> {
        self.model_bound_lens.get(model).copied()
    }

    /// 尚无任何显式或自动质量长度时，允许首张 292/332 质量票确定账号档位。
    pub fn allows_auto_quality_discovery(&self, model: &str) -> bool {
        self.model_bound_len(model).is_none()
            && self.bound_token_len.is_none()
            && self.auto_quality_len.is_none()
    }

    pub fn is_bound_to_account(&self, account_id: &str) -> bool {
        self.account_id.as_deref() == Some(account_id.trim())
    }

    /// 设置全局绑定长度（传 None 恢复账号自动识别的 292/332）。
    /// 从 pool 中提升匹配的 token 到 tokens（仅影响没有模型级覆盖的模型）。
    pub fn set_bound_len(&mut self, len: Option<usize>) {
        self.bound_token_len = len;
        self.promote_all_from_pool();
        self.persist();
    }

    /// 设置模型级绑定覆盖（传 None 清除覆盖，回退到全局）。
    pub fn set_model_bound_len(&mut self, model: &str, len: Option<usize>) {
        match len {
            Some(l) => { self.model_bound_lens.insert(model.to_string(), l); }
            None => { self.model_bound_lens.remove(model); }
        }
        // 仅提升该模型的 token
        let target = self.bound_len_for(model);
        let best = self.pool.get(model).and_then(|entries| {
            entries
                .iter()
                .filter(|ts| is_matching_token_len(ts.len, target))
                .filter(|ts| self.is_fresh_issued(ts.issued_unix))
                .max_by_key(|ts| ts.issued_unix)
                .cloned()
        });
        match best {
            Some(ts) => { self.tokens.insert(model.to_string(), ts); }
            None => { self.tokens.remove(model); }
        }
        self.persist();
    }

    /// 从 pool 提升所有模型的匹配 token 到 tokens。
    fn promote_all_from_pool(&mut self) {
        let promotions: Vec<(String, Option<TurnState>)> = self
            .pool
            .iter()
            .map(|(model, entries)| {
                let target = self.bound_len_for(model);
                let best = entries
                    .iter()
                    .filter(|ts| is_matching_token_len(ts.len, target))
                    .filter(|ts| self.is_fresh_issued(ts.issued_unix))
                    .max_by_key(|ts| ts.issued_unix)
                    .cloned();
                (model.clone(), best)
            })
            .collect();
        for (model, maybe_ts) in promotions {
            match maybe_ts {
                Some(ts) => { self.tokens.insert(model, ts); }
                None => { self.tokens.remove(&model); }
            }
        }
    }

    /// 记录某个模型最近一轮 fetch 的 token 长度分布
    pub fn record_distribution(&mut self, model: &str, dist: Vec<TokenLenCount>) {
        self.distributions.insert(model.to_string(), dist);
    }

    /// 获取某个模型的分布数据
    pub fn get_distribution(&self, model: &str) -> Vec<TokenLenCount> {
        self.distributions.get(model).cloned().unwrap_or_default()
    }

    // ─── Token 操作 ────────────────────────────────────────────

    pub fn set_reuse_policy(&mut self, policy: TokenReusePolicy) {
        self.reuse_policy = policy;
    }

    pub fn set_lifetime(&mut self, max_age_secs: i64, prefetch_age_secs: i64) {
        self.max_age_secs = max_age_secs.max(30);
        self.prefetch_age_secs = prefetch_age_secs.clamp(1, self.max_age_secs.saturating_sub(1));
    }

    fn age_within(&self, issued_unix: i64, limit: i64) -> bool {
        let age = now_unix().saturating_sub(issued_unix);
        (-MAX_FUTURE_SKEW_SECS..=limit).contains(&age)
    }

    fn is_fresh_issued(&self, issued_unix: i64) -> bool {
        self.age_within(issued_unix, self.max_age_secs)
    }

    /// 显式绑定其他长度的模型仍独立取票；332 和其他长度不跨模型共享。
    pub fn shares_292_for(&self, model: &str) -> bool {
        self.reuse_policy == TokenReusePolicy::Shared292
            && self.bound_len_for(model) == QUALITY_TOKEN_LEN
    }

    fn shared_292_state(&self) -> Option<(&str, &TurnState)> {
        // 无账号归属的旧缓存不可用于跨模型共享；绑定账号时会清掉旧缓存。
        self.account_id.as_ref()?;
        let now = now_unix();
        self.pool.iter()
            .flat_map(|(model, entries)| entries.iter().map(move |ts| (model.as_str(), ts)))
            .filter(|(_, ts)| ts.len == QUALITY_TOKEN_LEN && ts.token.len() == QUALITY_TOKEN_LEN)
            .filter(|(_, ts)| issued_unix(&ts.token) == Some(ts.issued_unix))
            .filter(|(_, ts)| now.saturating_sub(ts.issued_unix) >= -MAX_FUTURE_SKEW_SECS)
            .max_by(|(a_model, a), (b_model, b)| {
                a.issued_unix.cmp(&b.issued_unix)
                    .then_with(|| a.captured_at.cmp(&b.captured_at))
                    .then_with(|| a_model.cmp(b_model))
            })
    }

    fn state_for_model(&self, model: &str) -> Option<(&str, &TurnState)> {
        if self.shares_292_for(model) && self.account_id.is_some() {
            self.shared_292_state()
        } else {
            self.tokens.get_key_value(model).map(|(model, ts)| (model.as_str(), ts))
        }
    }

    /// 按复用策略取票（不消费）。仅返回未过期的 token。
    pub fn peek_for_model(&self, model: &str) -> Option<String> {
        self.state_for_model(model).and_then(|(_, ts)| {
            self.is_fresh_issued(ts.issued_unix).then(|| ts.token.clone())
        })
    }

    /// 仅用于测试；业务请求必须通过 peek_for_model 遵循复用策略与绑定长度。
    /// 返回任意模型中最新的有效 token。
    #[allow(dead_code)]
    pub fn peek_freshest(&self) -> Option<String> {
        self.tokens
            .values()
            .filter(|ts| self.is_fresh_issued(ts.issued_unix) && !is_degraded_token(&ts.token))
            .max_by_key(|ts| ts.issued_unix)
            .map(|ts| ts.token.clone())
    }

    /// 判断某个模型是否需要刷新凭据包：
    /// - 无 token → 需要
    /// - 年龄达到采集间隔（默认 30 秒）→ 再采一张
    pub fn needs_refresh(&self, model: &str) -> bool {
        match self.state_for_model(model) {
            None => true,
            Some((_, ts)) => {
                now_unix().saturating_sub(ts.issued_unix) >= self.prefetch_age_secs
                    || !self.is_fresh_issued(ts.issued_unix)
            }
        }
    }

    /// 清除所有模型的 token 和缓存池（服务端拒绝/降智时调用）。
    /// 注意：不清除 active_models 追踪，以便立即重新获取。
    pub fn invalidate_all(&mut self) {
        self.tokens.clear();
        self.pool.clear();
        self.persist();
    }

    /// 共享票被拒绝时移除所有来源的 292，防止切换来源后继续复用旧票。
    /// 模型隔离模式及非 292 绑定仍只清除对应模型。
    pub fn invalidate_model(&mut self, model: &str) {
        if self.shares_292_for(model) && self.account_id.is_some() {
            self.tokens.retain(|_, ts| ts.len != QUALITY_TOKEN_LEN);
            for entries in self.pool.values_mut() {
                entries.retain(|ts| ts.len != QUALITY_TOKEN_LEN);
            }
        } else {
            self.tokens.remove(model);
            self.pool.remove(model);
        }
        self.persist();
    }

    /// 绑定当前 ChatGPT 账号。账号变化时清空旧 Token、模型追踪和分布。
    /// 返回 true 表示发生了账号切换（或首次从无账号升级到有账号且已有残留）。
    pub fn bind_account(&mut self, account_id: &str) -> bool {
        let account_id = account_id.trim();
        if account_id.is_empty() {
            return false;
        }
        if self.account_id.as_deref() == Some(account_id) {
            return false;
        }
        let had_leftovers = self.account_id.is_some()
            || !self.tokens.is_empty()
            || !self.pool.is_empty()
            || !self.active_models.is_empty();
        if had_leftovers {
            eprintln!(
                "[account] 账号从 {:?} 切换到 {}，清空旧 Token 与模型追踪",
                self.account_id, account_id
            );
            self.tokens.clear();
            self.pool.clear();
            self.active_models.clear();
            self.distributions.clear();
            self.model_bound_lens.clear();
            self.bound_token_len = None;
            self.auto_quality_len = None;
        }
        self.account_id = Some(account_id.to_string());
        self.persist();
        had_leftovers
    }

    /// 将 token 存入缓存池（不管长度是否匹配绑定）。
    /// 每个 (model, len_bucket) 只保留最新的一个。
    pub fn store_to_pool(&mut self, model: &str, token: &str, source: &str) -> bool {
        self.store_to_pool_with_session(model, token, source, None, None, &[])
    }

    fn store_to_pool_with_session(
        &mut self,
        model: &str,
        token: &str,
        source: &str,
        proxy_session: Option<&str>,
        previous_response_id: Option<&str>,
        routing_cookies: &[RoutingCookie],
    ) -> bool {
        let Some(mut state) = TurnState::from_token(token, source) else {
            return false;
        };
        if let Some(session) = proxy_session.map(str::trim).filter(|value| {
            !value.is_empty()
                && value.len() <= 32
                && value.chars().all(|ch| ch.is_ascii_alphanumeric())
        }) {
            state.proxy_session = session.to_string();
        }
        if let Some(response_id) = previous_response_id.map(str::trim).filter(|value| {
            !value.is_empty() && value.len() <= 128 && value.chars().all(|ch| ch.is_ascii_graphic())
        }) {
            state.previous_response_id = response_id.to_string();
        }
        state.routing_cookies = chatgpt_cookies::sanitize_list(routing_cookies);
        let entries = self.pool.entry(model.to_string()).or_default();
        // 替换同长度区间的旧 token（±4 范围算同一种）
        if let Some(pos) = entries
            .iter()
            .position(|e| is_matching_token_len(e.len, state.len))
        {
            entries[pos] = state.clone();
        } else {
            entries.push(state.clone());
        }
        // An explicit model override must not change the account-wide target.
        if self.model_bound_len(model).is_none() {
            self.observe_quality_len(state.len);
        }
        // 如果匹配该模型的绑定长度，同时更新 tokens（活跃注入用）
        let target = self.bound_len_for(model);
        if is_matching_token_len(state.len, target) {
            self.tokens.insert(model.to_string(), state);
        }
        true
    }

    /// 批量将一组 token 全部入池，然后一次性持久化。
    /// 返回匹配绑定长度的 token 数量。
    pub fn capture_batch(&mut self, model: &str, tokens: &[String], source: &str) -> usize {
        let mut matched = 0;
        for token in tokens {
            if self.store_to_pool(model, token, source) {
                let len = token.trim().len();
                if is_matching_token_len(len, self.bound_len_for(model)) {
                    matched += 1;
                }
            }
        }
        self.persist();
        matched
    }

    fn observe_quality_len(&mut self, len: usize) {
        let Some(canon) = canonical_quality_len(len) else {
            return;
        };
        if self.auto_quality_len == Some(canon) {
            return;
        }
        eprintln!("[token] 账号质量长度识别为 {canon}");
        self.auto_quality_len = Some(canon);
        if self.bound_token_len.is_none() {
            self.promote_all_from_pool();
        }
    }

    /// 兼容旧接口：存入匹配绑定长度的 token 到 tokens + pool。
    pub fn capture(&mut self, model: &str, token: &str, source: &str) -> bool {
        self.capture_with_session(model, token, source, None, None, &[])
    }

    pub fn capture_with_session(
        &mut self,
        model: &str,
        token: &str,
        source: &str,
        proxy_session: Option<&str>,
        previous_response_id: Option<&str>,
        routing_cookies: &[RoutingCookie],
    ) -> bool {
        let ok = self.store_to_pool_with_session(
            model,
            token,
            source,
            proxy_session,
            previous_response_id,
            routing_cookies,
        );
        if ok {
            self.persist();
        }
        ok
    }

    pub fn bound_proxy_session(&self) -> Option<String> {
        if let Some((_, ts)) = self.shared_292_state() {
            if !ts.proxy_session.is_empty() && self.is_fresh_issued(ts.issued_unix) {
                return Some(ts.proxy_session.clone());
            }
        }
        self.tokens
            .values()
            .filter(|ts| !ts.proxy_session.is_empty() && !is_degraded_token(&ts.token))
            .filter(|ts| self.is_fresh_issued(ts.issued_unix))
            .max_by_key(|ts| ts.issued_unix)
            .map(|ts| ts.proxy_session.clone())
    }

    pub fn proxy_session_for_token(&self, token: Option<&str>) -> Option<String> {
        if let Some(token) = token.map(str::trim).filter(|value| !value.is_empty()) {
            let found = self
                .tokens
                .values()
                .chain(self.pool.values().flatten())
                .find(|ts| ts.token == token && !ts.proxy_session.is_empty())
                .map(|ts| ts.proxy_session.clone());
            if found.is_some() {
                return found;
            }
        }
        self.bound_proxy_session()
    }

    pub fn previous_response_id_for_token(&self, token: Option<&str>) -> Option<String> {
        let token = token.map(str::trim).filter(|value| !value.is_empty())?;
        self.tokens
            .values()
            .chain(self.pool.values().flatten())
            .find(|ts| ts.token == token && !ts.previous_response_id.is_empty())
            .map(|ts| ts.previous_response_id.clone())
    }

    pub fn bound_routing_cookies(&self) -> Vec<RoutingCookie> {
        let now = now_unix();
        if let Some((_, ts)) = self.shared_292_state() {
            if self.is_fresh_issued(ts.issued_unix) {
                return chatgpt_cookies::live_cookies(&ts.routing_cookies, now);
            }
        }
        self.tokens
            .values()
            .filter(|ts| !is_degraded_token(&ts.token) && self.is_fresh_issued(ts.issued_unix))
            .max_by_key(|ts| ts.issued_unix)
            .map(|ts| chatgpt_cookies::live_cookies(&ts.routing_cookies, now))
            .unwrap_or_default()
    }

    pub fn routing_cookies_for_token(&self, token: Option<&str>) -> Vec<RoutingCookie> {
        let now = now_unix();
        if let Some(token) = token.map(str::trim).filter(|value| !value.is_empty()) {
            if let Some(ts) = self
                .tokens
                .values()
                .chain(self.pool.values().flatten())
                .find(|ts| ts.token == token)
            {
                return chatgpt_cookies::live_cookies(&ts.routing_cookies, now);
            }
        }
        self.bound_routing_cookies()
    }

    /// 获取某模型在池中所有有效 token 的长度分类。
    /// 返回按长度排序的 [(len, is_bound_match, age_secs)]。
    pub fn pool_summary(&self, model: &str) -> Vec<(usize, bool, i64)> {
        let target = self.bound_len_for(model);
        let now = now_unix();
        let mut result: Vec<(usize, bool, i64)> = self
            .pool
            .get(model)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|ts| now - ts.issued_unix <= self.max_age_secs)
                    .map(|ts| {
                        let age = now - ts.issued_unix;
                        (ts.len, is_matching_token_len(ts.len, target), age)
                    })
                    .collect()
            })
            .unwrap_or_default();
        result.sort_by_key(|&(len, _, _)| len);
        result
    }

    /// 清理池中过期的 token，防止内存膨胀。
    /// - 移除超过 POOL_MAX_AGE_SECS（2小时）的 token
    /// - 每模型最多保留 MAX_POOL_VARIANTS_PER_MODEL 种长度（保最新的）
    /// - 移除空模型条目
    pub fn cleanup_pool(&mut self) {
        let now = now_unix();
        let mut removed = 0usize;

        for entries in self.pool.values_mut() {
            let before = entries.len();
            // 清除过期 token
            entries.retain(|ts| {
                let age = now - ts.issued_unix;
                age <= POOL_MAX_AGE_SECS
            });
            removed += before - entries.len();

            // 如果某个模型的 token 种类超出上限，保留最新的 N 个
            if entries.len() > MAX_POOL_VARIANTS_PER_MODEL {
                entries.sort_by(|a, b| b.issued_unix.cmp(&a.issued_unix));
                let excess = entries.len() - MAX_POOL_VARIANTS_PER_MODEL;
                entries.truncate(MAX_POOL_VARIANTS_PER_MODEL);
                removed += excess;
            }
        }

        // 清除空模型条目
        self.pool.retain(|_, v| !v.is_empty());

        if removed > 0 {
            eprintln!("[pool] 清理了 {} 个过期/多余的缓存 token", removed);
        }
    }

    /// 所有模型中有效 token 的数量
    pub fn fresh_count(&self) -> usize {
        self.tokens
            .values()
            .filter(|ts| self.is_fresh_issued(ts.issued_unix) && !is_degraded_token(&ts.token))
            .count()
    }

    /// 仅用于测试；业务注入请使用 peek_for_model。
    #[allow(dead_code)]
    pub fn stampable(&self) -> Option<String> {
        self.peek_freshest()
    }

    // ─── 视图 ──────────────────────────────────────────────────

    /// 生成状态视图，自动展示所有活跃模型。
    pub fn view(&self) -> TurnStateView {
        let active = self.all_active_models();
        self.view_for_models(&active)
    }

    /// 基于指定模型列表生成视图（内部方法，也用于带 seed 的场景）。
    fn view_for_models(&self, models: &[String]) -> TurnStateView {
        let now = now_unix();
        let model_views: Vec<ModelTokenView> = models
            .iter()
            .map(|model| {
                let dist = self.get_distribution(model);
                let model_target = self.bound_len_for(model);
                let model_override = self.model_bound_len(model);

                // 收集池中该模型的所有 token 信息
                let pool_tokens: Vec<PoolTokenInfo> = self
                    .pool
                    .get(model.as_str())
                    .map(|entries| {
                        let mut infos: Vec<PoolTokenInfo> = entries
                            .iter()
                            .map(|ts| {
                                let age = now - ts.issued_unix;
                                PoolTokenInfo {
                                    len: ts.len,
                                    age_secs: age,
                                    is_bound: is_matching_token_len(ts.len, model_target),
                                    is_valid: age <= self.max_age_secs,
                                }
                            })
                            .collect();
                        infos.sort_by_key(|i| i.len);
                        infos
                    })
                    .unwrap_or_default();

                match self.state_for_model(model) {
                    Some((source_model, state)) => {
                        let age = now - state.issued_unix;
                        let status = if age > self.max_age_secs {
                            "expired"
                        } else if age >= self.prefetch_age_secs {
                            "refreshing"
                        } else {
                            "active"
                        };
                        ModelTokenView {
                            model: model.clone(),
                            status: status.into(),
                            age_secs: Some(age),
                            len: Some(state.len),
                            captured_at: Some(state.captured_at.clone())
                                .filter(|v| !v.is_empty()),
                            distribution: dist,
                            pool_tokens,
                            bound_override: model_override,
                            shared_from_model: (self.shares_292_for(model) && self.account_id.is_some())
                                .then(|| source_model.to_string()),
                        }
                    }
                    None => ModelTokenView {
                        model: model.clone(),
                        status: "empty".into(),
                        age_secs: None,
                        len: None,
                        captured_at: None,
                        distribution: dist,
                        pool_tokens,
                        bound_override: model_override,
                        shared_from_model: None,
                    },
                }
            })
            .collect();

        let overall_status = if model_views.is_empty() {
            "idle"
        } else if model_views.iter().all(|v| v.status == "active") {
            "active"
        } else if model_views
            .iter()
            .any(|v| v.status == "active" || v.status == "refreshing")
        {
            "partial"
        } else {
            "empty"
        };

        let shared_state = if models.iter().any(|model| self.shares_292_for(model)) {
            self.shared_292_state()
        } else {
            None
        };
        // The headline describes the shared ticket, not a newer independent
        // 332 override whose age could conceal an aging shared pool.
        let freshest = shared_state.map(|(_, ts)| ts).or_else(|| {
            models.iter()
                .filter_map(|model| self.state_for_model(model).map(|(_, ts)| ts))
                .max_by_key(|ts| ts.issued_unix)
        });
        let shared_source = shared_state
            .filter(|(_, ts)| now.saturating_sub(ts.issued_unix) <= self.max_age_secs)
            .map(|(model, _)| model.to_string());

        TurnStateView {
            status: overall_status.into(),
            age_secs: freshest.map(|ts| now_unix() - ts.issued_unix),
            len: freshest.map(|ts| ts.len),
            source: freshest.and_then(|ts| Some(ts.source.clone()).filter(|v| !v.is_empty())),
            captured_at: freshest
                .and_then(|ts| Some(ts.captured_at.clone()).filter(|v| !v.is_empty())),
            models: model_views,
            bound_token_len: self.bound_len(),
            shared_source_model: shared_source,
            bound_proxy_session: self.bound_proxy_session(),
        }
    }
}

impl TurnState {
    pub fn from_token(token: &str, source: &str) -> Option<Self> {
        let token = token.trim();
        let issued_unix = issued_unix(token)?;
        Some(Self {
            token: token.to_string(),
            issued_unix,
            len: token.len(),
            source: source.to_string(),
            captured_at: now_rfc3339(),
            proxy_session: String::new(),
            previous_response_id: String::new(),
            routing_cookies: Vec::new(),
        })
    }
}

// ─── 公共工具函数 ──────────────────────────────────────────────

pub fn issued_unix(token: &str) -> Option<i64> {
    let token = token.trim();
    if !token.starts_with("gAAAAA") {
        return None;
    }
    let bytes = decode_fernet(token)?;
    if bytes.len() < 9 || bytes[0] != 0x80 {
        return None;
    }
    Some(i64::from_be_bytes(bytes[1..9].try_into().ok()?))
}

pub fn should_stamp_http(method: &str, path: &str) -> bool {
    method.eq_ignore_ascii_case("POST") && path.contains("/responses")
}

/// 上下文压缩请求保持原样转发，不包装成探测的第二包，也不替换 State。
pub fn is_context_compaction(path: &str, bytes: &[u8]) -> bool {
    path.contains("/responses/compact")
        || request_json(bytes).is_some_and(|value| value.get("compaction_trigger").is_some())
}

pub fn header_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(HEADER_NAME)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| value.starts_with("gAAAAA"))
        .map(str::to_string)
}

pub fn has_http_turn_state(headers: &HeaderMap) -> bool {
    headers
        .get(HEADER_NAME)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty())
}

pub fn has_body_turn_state(bytes: &[u8]) -> bool {
    request_json(bytes)
        .as_ref()
        .and_then(|value| value.pointer(&format!("/client_metadata/{HEADER_NAME}")))
        .and_then(Value::as_str)
        .is_some_and(|value| !value.trim().is_empty())
}

/// 下游请求已经是同轮续跑时，不再改写它自带的 previous_response_id。
pub fn is_same_turn_follow_up(bytes: &[u8]) -> bool {
    let Some(value) = request_json(bytes) else {
        return false;
    };
    if value
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty())
    {
        return true;
    }
    input_items(&value)
        .last()
        .is_some_and(is_tool_output_item)
}

fn request_json(bytes: &[u8]) -> Option<Value> {
    if let Ok(value) = serde_json::from_slice(bytes) {
        return Some(value);
    }
    decode_request_json_bytes(bytes).and_then(|plain| serde_json::from_slice(&plain).ok())
}

fn decode_request_json_bytes(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() >= 4 && bytes[0] == 0x28 && bytes[1] == 0xB5 && bytes[2] == 0x2F && bytes[3] == 0xFD {
        if let Ok(plain) = zstd::decode_all(std::io::Cursor::new(bytes)) {
            return Some(plain);
        }
    }
    if bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B {
        if let Some(plain) = decompress_named(bytes, "gzip") {
            return Some(plain);
        }
    }
    decompress_named(bytes, "deflate").or_else(|| decompress_named(bytes, "raw_deflate"))
}

fn input_items(value: &Value) -> &[Value] {
    value
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn is_tool_output_item(item: &Value) -> bool {
    match item.get("type").and_then(Value::as_str) {
        Some(
            "function_call_output"
            | "custom_tool_call_output"
            | "computer_call_output"
            | "local_shell_call_output"
            | "shell_call_output"
            | "mcp_tool_call_output"
            | "tool_result",
        ) => true,
        _ => item.get("role").and_then(Value::as_str) == Some("tool"),
    }
}

pub fn apply_http_header(headers: &mut HeaderMap, token: &str) {
    let Some(token) = injectable_http_token(token) else {
        return;
    };
    if let (Ok(name), Ok(value)) = (
        HeaderName::from_bytes(HEADER_NAME.as_bytes()),
        HeaderValue::from_str(&token),
    ) {
        headers.insert(name, value);
    }
}

/// 只允许把可解析、非降级的 Fernet 票据写入请求头，避免补一个上游会拒的假值。
pub fn injectable_http_token(token: &str) -> Option<String> {
    let token = token.trim();
    if token.is_empty() || is_degraded_token(token) {
        return None;
    }
    TurnState::from_token(token, "").map(|state| state.token)
}

pub fn clear_http_header(headers: &mut HeaderMap) {
    headers.remove(HEADER_NAME);
}

pub fn ws_looks_json_object(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .is_some_and(|value| value.is_object())
}

pub fn ws_has_turn_state(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    match value.pointer(&format!("/client_metadata/{HEADER_NAME}")) {
        Some(Value::String(token)) => !token.is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

pub fn stamp_ws_json(text: &str, token: &str) -> Option<String> {
    let mut value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object_mut()?;
    let metadata = object.entry("client_metadata").or_insert_with(|| json!({}));
    let metadata = metadata.as_object_mut()?;
    metadata.insert(HEADER_NAME.to_string(), json!(token));
    Some(value.to_string())
}

pub fn token_from_json(text: &str) -> Option<String> {
    let value: Value = serde_json::from_str(text).ok()?;
    find_token(&value)
}

/// 从 JSON 请求体中提取 model 字段（自动处理 zstd/gzip/deflate 压缩）
pub fn extract_model_from_body(bytes: &[u8]) -> Option<String> {
    // 先尝试原始字节（未压缩 JSON）
    if let Some(model) = extract_model_from_json(bytes) {
        return Some(model);
    }
    // zstd（magic: 0x28 0xB5 0x2F 0xFD）—— Codex CLI 默认使用 zstd
    if bytes.len() >= 4 && bytes[0] == 0x28 && bytes[1] == 0xB5 && bytes[2] == 0x2F && bytes[3] == 0xFD {
        if let Ok(decompressed) = zstd::decode_all(std::io::Cursor::new(bytes)) {
            if let Some(model) = extract_model_from_json(&decompressed) {
                return Some(model);
            }
        }
    }
    // gzip（magic: 0x1f 0x8b）
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        if let Some(model) = try_decompress_and_extract(bytes, "gzip") {
            return Some(model);
        }
    }
    // deflate/zlib
    if let Some(model) = try_decompress_and_extract(bytes, "deflate") {
        return Some(model);
    }
    // raw deflate
    if let Some(model) = try_decompress_and_extract(bytes, "raw_deflate") {
        return Some(model);
    }
    None
}

/// 将 JSON 请求体中的 `model` 改成指定值。压缩体按原编码写回。
pub fn rewrite_model_in_body(
    bytes: &[u8],
    encoding: Option<&str>,
    model: &str,
) -> Result<Vec<u8>, String> {
    let model = model.trim();
    if model.is_empty() {
        return Ok(bytes.to_vec());
    }
    if extract_model_from_json(bytes).as_deref() == Some(model) {
        return Ok(bytes.to_vec());
    }
    match detect_body_compression(bytes, encoding) {
        None => rewrite_json_model(bytes, model),
        Some("zstd") => {
            let plain = zstd::decode_all(std::io::Cursor::new(bytes))
                .map_err(|_| "无法解压 zstd 请求体，不能强制绑定模型".to_string())?;
            if extract_model_from_json(&plain).as_deref() == Some(model) {
                return Ok(bytes.to_vec());
            }
            let rewritten = rewrite_json_model(&plain, model)?;
            zstd::encode_all(rewritten.as_slice(), 0)
                .map_err(|_| "无法重新压缩 zstd 请求体".to_string())
        }
        Some("gzip") => {
            let plain = decompress_named(bytes, "gzip")
                .ok_or_else(|| "无法解压 gzip 请求体，不能强制绑定模型".to_string())?;
            if extract_model_from_json(&plain).as_deref() == Some(model) {
                return Ok(bytes.to_vec());
            }
            compress_gzip(&rewrite_json_model(&plain, model)?)
        }
        Some("deflate") => {
            let plain = decompress_named(bytes, "deflate")
                .or_else(|| decompress_named(bytes, "raw_deflate"))
                .ok_or_else(|| "无法解压 deflate 请求体，不能强制绑定模型".to_string())?;
            if extract_model_from_json(&plain).as_deref() == Some(model) {
                return Ok(bytes.to_vec());
            }
            compress_deflate(&rewrite_json_model(&plain, model)?)
        }
        Some(_) => Err("不支持的请求体压缩，不能强制绑定模型".into()),
    }
}

/// 把下游请求包装成带 State 的同轮续跑：只写 client_metadata 里的票据。
/// Codex HTTP `/responses` 不接受 `previous_response_id`，转发前会去掉该字段。
pub fn wrap_as_second_packet(
    bytes: &[u8],
    encoding: Option<&str>,
    token: &str,
) -> Result<Vec<u8>, String> {
    let Some(token) = injectable_http_token(token).or_else(|| {
        let token = token.trim();
        (!token.is_empty()).then(|| token.to_string())
    }) else {
        return Ok(bytes.to_vec());
    };
    match detect_body_compression(bytes, encoding) {
        None => wrap_json_as_second_packet(bytes, &token),
        Some("zstd") => {
            let plain = zstd::decode_all(std::io::Cursor::new(bytes))
                .map_err(|_| "无法解压 zstd 请求体，不能包装为同轮第二包".to_string())?;
            let rewritten = wrap_json_as_second_packet(&plain, &token)?;
            zstd::encode_all(rewritten.as_slice(), 0)
                .map_err(|_| "无法重新压缩 zstd 请求体".to_string())
        }
        Some("gzip") => {
            let plain = decompress_named(bytes, "gzip")
                .ok_or_else(|| "无法解压 gzip 请求体，不能包装为同轮第二包".to_string())?;
            compress_gzip(&wrap_json_as_second_packet(&plain, &token)?)
        }
        Some("deflate") => {
            let plain = decompress_named(bytes, "deflate")
                .or_else(|| decompress_named(bytes, "raw_deflate"))
                .ok_or_else(|| "无法解压 deflate 请求体，不能包装为同轮第二包".to_string())?;
            compress_deflate(&wrap_json_as_second_packet(&plain, &token)?)
        }
        Some(_) => Err("不支持的请求体压缩，不能包装为同轮第二包".into()),
    }
}

fn wrap_json_as_second_packet(bytes: &[u8], token: &str) -> Result<Vec<u8>, String> {
    let mut value: Value = serde_json::from_slice(bytes)
        .map_err(|_| "请求体不是 JSON，不能包装为同轮第二包".to_string())?;
    let Some(object) = value.as_object_mut() else {
        return Err("请求体不是 JSON 对象，不能包装为同轮第二包".into());
    };
    object.remove("previous_response_id");
    let metadata = object
        .entry("client_metadata")
        .or_insert_with(|| json!({}));
    if let Some(metadata) = metadata.as_object_mut() {
        metadata.insert(HEADER_NAME.to_string(), json!(token));
    }
    serde_json::to_vec(&value).map_err(|_| "无法序列化包装后的请求体".to_string())
}

fn detect_body_compression(bytes: &[u8], encoding: Option<&str>) -> Option<&'static str> {
    let encoding = encoding.unwrap_or("").to_ascii_lowercase();
    if encoding.contains("zstd")
        || (bytes.len() >= 4 && bytes[0] == 0x28 && bytes[1] == 0xB5 && bytes[2] == 0x2F && bytes[3] == 0xFD)
    {
        return Some("zstd");
    }
    if encoding.contains("gzip") || (bytes.len() >= 2 && bytes[0] == 0x1F && bytes[1] == 0x8B) {
        return Some("gzip");
    }
    if encoding.contains("deflate") {
        return Some("deflate");
    }
    None
}

fn rewrite_json_model(bytes: &[u8], model: &str) -> Result<Vec<u8>, String> {
    let mut value: Value =
        serde_json::from_slice(bytes).map_err(|_| "请求体不是 JSON，不能强制绑定模型".to_string())?;
    let Some(object) = value.as_object_mut() else {
        return Err("请求体不是 JSON 对象，不能强制绑定模型".into());
    };
    object.insert("model".into(), Value::String(model.to_string()));
    serde_json::to_vec(&value).map_err(|_| "无法序列化改写后的请求体".to_string())
}

fn decompress_named(bytes: &[u8], method: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut decompressed = Vec::new();
    let ok = match method {
        "gzip" => flate2::read::GzDecoder::new(bytes)
            .read_to_end(&mut decompressed)
            .is_ok(),
        "deflate" => flate2::read::ZlibDecoder::new(bytes)
            .read_to_end(&mut decompressed)
            .is_ok(),
        "raw_deflate" => flate2::read::DeflateDecoder::new(bytes)
            .read_to_end(&mut decompressed)
            .is_ok(),
        _ => false,
    };
    (ok && !decompressed.is_empty()).then_some(decompressed)
}

fn compress_gzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(bytes)
        .and_then(|_| encoder.finish())
        .map_err(|_| "无法重新压缩 gzip 请求体".to_string())
}

fn compress_deflate(bytes: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Write;
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder
        .write_all(bytes)
        .and_then(|_| encoder.finish())
        .map_err(|_| "无法重新压缩 deflate 请求体".to_string())
}

fn extract_model_from_json(bytes: &[u8]) -> Option<String> {
    // JSON 解析
    if let Ok(v) = serde_json::from_slice::<Value>(bytes) {
        if let Some(model) = v.get("model").and_then(|v| v.as_str()).map(str::to_string) {
            return Some(model);
        }
    }
    // 后备：在 UTF-8 文本中搜索 "model":"..."
    let text = std::str::from_utf8(bytes).ok()?;
    let needle = "\"model\"";
    let idx = text.find(needle)?;
    let after = &text[idx + needle.len()..];
    let colon_pos = after.find(':')?;
    let after_colon = after[colon_pos + 1..].trim_start();
    if after_colon.starts_with('"') {
        let end = after_colon[1..].find('"')?;
        let model = &after_colon[1..1 + end];
        if !model.is_empty() {
            return Some(model.to_string());
        }
    }
    None
}

fn try_decompress_and_extract(bytes: &[u8], method: &str) -> Option<String> {
    use std::io::Read;
    let mut decompressed = Vec::new();
    let ok = match method {
        "gzip" => {
            let mut decoder = flate2::read::GzDecoder::new(bytes);
            decoder.read_to_end(&mut decompressed).is_ok()
        }
        "deflate" => {
            let mut decoder = flate2::read::ZlibDecoder::new(bytes);
            decoder.read_to_end(&mut decompressed).is_ok()
        }
        "raw_deflate" => {
            let mut decoder = flate2::read::DeflateDecoder::new(bytes);
            decoder.read_to_end(&mut decompressed).is_ok()
        }
        _ => false,
    };
    if ok && !decompressed.is_empty() {
        extract_model_from_json(&decompressed)
    } else {
        None
    }
}

fn find_token(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key.eq_ignore_ascii_case(HEADER_NAME) {
                    if let Some(token) = child
                        .as_str()
                        .map(str::trim)
                        .filter(|value| value.starts_with("gAAAAA"))
                    {
                        return Some(token.to_string());
                    }
                }
                if let Some(token) = find_token(child) {
                    return Some(token);
                }
            }
            None
        }
        Value::Array(items) => items.iter().find_map(find_token),
        _ => None,
    }
}

fn decode_fernet(token: &str) -> Option<Vec<u8>> {
    URL_SAFE
        .decode(token)
        .ok()
        .or_else(|| URL_SAFE_NO_PAD.decode(token).ok())
        .or_else(|| {
            let mut padded = token.to_string();
            while padded.len() % 4 != 0 {
                padded.push('=');
            }
            URL_SAFE.decode(padded).ok()
        })
}

fn now_unix() -> i64 {
    Utc::now().timestamp()
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// 判断 token 长度是否与目标长度匹配（±4 容差）
pub fn is_matching_token_len(actual: usize, target: usize) -> bool {
    let low = target.saturating_sub(4);
    let high = target + 4;
    (low..=high).contains(&actual)
}

pub fn canonical_quality_len(len: usize) -> Option<usize> {
    if is_matching_token_len(len, QUALITY_TOKEN_LEN) {
        Some(QUALITY_TOKEN_LEN)
    } else if is_matching_token_len(len, QUALITY_TOKEN_LEN_332) {
        Some(QUALITY_TOKEN_LEN_332)
    } else {
        None
    }
}

fn infer_auto_quality_len(
    tokens: &HashMap<String, TurnState>,
    pool: &HashMap<String, Vec<TurnState>>,
) -> Option<usize> {
    tokens
        .values()
        .map(|ts| ts.len)
        .chain(pool.values().flat_map(|entries| entries.iter().map(|ts| ts.len)))
        .find_map(canonical_quality_len)
}

pub fn is_degraded_token(token: &str) -> bool {
    let len = token.trim().len();
    (308..=316).contains(&len)
}

pub fn is_quality_token(token: &str) -> bool {
    canonical_quality_len(token.trim().len()).is_some()
}

pub fn token_quality_label(token: &str) -> &'static str {
    match canonical_quality_len(token.trim().len()) {
        Some(QUALITY_TOKEN_LEN) => "292/normal",
        Some(QUALITY_TOKEN_LEN_332) => "332/normal",
        _ if is_degraded_token(token) => "312/degraded",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token_for(issued: i64) -> String {
        token_for_len(issued, QUALITY_TOKEN_LEN)
    }

    /// 生成指定 base64 编码后长度接近 target_len 的 token
    fn token_for_len(issued: i64, target_len: usize) -> String {
        let mut raw = vec![0x80];
        raw.extend_from_slice(&issued.to_be_bytes());
        // base64 编码后长度 ≈ ceil(raw_len / 3) * 4
        // 要达到 target_len 的 base64，需要 raw_len ≈ target_len * 3 / 4
        let needed_raw = (target_len * 3 / 4).saturating_sub(raw.len());
        raw.extend_from_slice(&vec![0u8; needed_raw]);
        URL_SAFE.encode(raw)
    }

    fn shared_store() -> TurnStateStore {
        let mut store = TurnStateStore::default();
        store.bind_account("shared-test-account");
        store.set_bound_len(Some(QUALITY_TOKEN_LEN));
        store
    }

    #[test]
    fn shared_292_serves_existing_and_new_models_without_copying_caches() {
        let mut store = shared_store();
        store.register_model("donor");
        store.register_model("consumer");
        let token = token_for(now_unix() - 20);
        assert!(store.capture("donor", &token, "fetch"));
        assert_eq!(store.peek_for_model("consumer").as_deref(), Some(token.as_str()));
        store.register_model("new-model");
        assert!(!store.needs_refresh("new-model"));
        assert_eq!(store.tokens.len(), 1);
        assert_eq!(store.pool.len(), 1);
        let view = store.view();
        assert_eq!(view.status, "active");
        assert_eq!(view.shared_source_model.as_deref(), Some("donor"));
        assert!(view.models.iter().all(|m| m.shared_from_model.as_deref() == Some("donor")));
        assert!(view.models.iter().find(|m| m.model == "consumer").unwrap().pool_tokens.is_empty());

        store.set_reuse_policy(TokenReusePolicy::PerModel);
        assert!(store.peek_for_model("consumer").is_none());
        assert!(store.needs_refresh("consumer"));
        assert_eq!(store.peek_for_model("donor").as_deref(), Some(token.as_str()));
        assert_eq!(store.view().status, "partial");
        assert!(store.view().shared_source_model.is_none());
        store.set_reuse_policy(TokenReusePolicy::Shared292);
        assert_eq!(store.peek_for_model("consumer").as_deref(), Some(token.as_str()));
        assert_eq!(store.pool.len(), 1);
    }

    #[test]
    fn shared_292_uses_newest_donor_and_one_refresh_window() {
        let mut store = shared_store();
        let older = token_for(now_unix() - 120);
        store.capture("a", &older, "fetch");
        assert!(store.needs_refresh("b"));
        assert_eq!(store.peek_for_model("b").as_deref(), Some(older.as_str()));
        let fresh = token_for(now_unix() - 10);
        store.capture("c", &fresh, "fetch");
        for model in ["a", "b", "c", "new"] {
            assert_eq!(store.peek_for_model(model).as_deref(), Some(fresh.as_str()));
            assert!(!store.needs_refresh(model));
        }
        store.invalidate_all();
        store.capture("a", &token_for(now_unix() - MAX_AGE_SECS - 10), "fetch");
        assert!(store.peek_for_model("b").is_none());
        assert!(store.needs_refresh("b"));
        store.register_model("b");
        assert_eq!(store.view().models[0].status, "expired");
    }

    #[test]
    fn shared_292_rejects_other_lengths_future_and_malformed_cached_tickets() {
        let mut store = shared_store();
        for len in [288, 296, 312, 332] {
            store.capture(&format!("source-{len}"), &token_for_len(now_unix() - 10, len), "fetch");
        }
        store.capture("future", &token_for(now_unix() + MAX_FUTURE_SKEW_SECS + 60), "fetch");
        store.pool.insert("bad-cache".into(), vec![TurnState {
            token: "gAAAAA".to_string() + &"!".repeat(286),
            len: QUALITY_TOKEN_LEN, issued_unix: now_unix(), source: "fetch".into(), captured_at: String::new(),
            proxy_session: String::new(),
            previous_response_id: String::new(),
            routing_cookies: Vec::new(),
        }]);
        assert!(store.peek_for_model("consumer").is_none());
        assert!(store.needs_refresh("consumer"));
        let token = token_for(now_unix() - 10);
        store.capture("donor", &token, "fetch");
        assert_eq!(store.peek_for_model("consumer").as_deref(), Some(token.as_str()));
    }

    #[test]
    fn shared_292_keeps_other_bindings_and_accounts_isolated() {
        let mut store = shared_store();
        let shared = token_for(now_unix() - 30);
        let independent = token_for_len(now_unix() - 10, QUALITY_TOKEN_LEN_332);
        store.capture("donor", &shared, "fetch");
        store.set_model_bound_len("independent", Some(QUALITY_TOKEN_LEN_332));
        assert!(store.peek_for_model("independent").is_none());
        store.capture("independent", &independent, "fetch");
        assert_eq!(store.peek_for_model("independent").as_deref(), Some(independent.as_str()));
        assert_eq!(store.peek_for_model("consumer").as_deref(), Some(shared.as_str()));
        store.set_model_bound_len("other-independent", Some(QUALITY_TOKEN_LEN_332));
        assert!(store.peek_for_model("other-independent").is_none());
        store.bind_account("another-account");
        assert!(store.peek_for_model("consumer").is_none());
        assert!(store.peek_for_model("independent").is_none());
        assert!(store.needs_refresh("new-model"));
        assert_eq!(store.reuse_policy, TokenReusePolicy::Shared292);
    }

    #[test]
    fn model_override_does_not_change_default_shared_target() {
        let mut store = TurnStateStore::default();
        store.bind_account("account");
        let token = token_for(now_unix() - 10);
        store.capture("donor", &token, "fetch");
        store.register_model("donor");
        store.register_model("independent");
        store.set_model_bound_len("independent", Some(QUALITY_TOKEN_LEN_332));
        store.capture("independent", &token_for_len(now_unix() - 5, QUALITY_TOKEN_LEN_332), "fetch");
        assert_eq!(store.bound_len(), QUALITY_TOKEN_LEN);
        assert_eq!(store.view().len, Some(QUALITY_TOKEN_LEN));
        assert_eq!(store.view().shared_source_model.as_deref(), Some("donor"));
        assert_eq!(store.peek_for_model("new-model").as_deref(), Some(token.as_str()));
        assert!(!store.needs_refresh("new-model"));
        store.set_reuse_policy(TokenReusePolicy::PerModel);
        assert!(store.peek_for_model("new-model").is_none());
        assert!(store.peek_for_model("independent").is_some());
    }

    #[test]
    fn shared_292_invalidation_clears_all_donors_not_other_lengths() {
        let mut store = shared_store();
        store.capture("a", &token_for(now_unix() - 30), "fetch");
        store.capture("b", &token_for(now_unix() - 60), "fetch");
        store.register_model("consumer");
        store.set_model_bound_len("independent", Some(QUALITY_TOKEN_LEN_332));
        store.capture("independent", &token_for_len(now_unix() - 10, QUALITY_TOKEN_LEN_332), "fetch");
        store.invalidate_model("consumer");
        for model in ["a", "b", "consumer"] {
            assert!(store.peek_for_model(model).is_none());
            assert!(store.needs_refresh(model));
        }
        assert!(store.peek_for_model("independent").is_some());
        assert!(store.all_active_models().contains(&"consumer".to_string()));
    }

    #[test]
    fn unowned_legacy_cache_does_not_become_shared() {
        let mut store = TurnStateStore::default();
        store.capture("source", &token_for(now_unix() - 30), "fetch");
        assert!(store.peek_for_model("other").is_none());
        store.bind_account("current-account");
        assert!(store.peek_for_model("other").is_none());
        assert!(store.peek_for_model("source").is_none());
    }

    #[test]
    fn parses_fernet_timestamp() {
        let issued = 1_700_000_000;
        let token = token_for(issued);
        assert!(token.starts_with("gAAAAA"));
        assert_eq!(issued_unix(&token), Some(issued));
    }

    #[test]
    fn rejects_non_token() {
        assert!(issued_unix("not-a-token").is_none());
        assert!(issued_unix("").is_none());
    }

    #[test]
    fn auto_discover_and_peek_for_model() {
        let mut store = TurnStateStore::default();
        assert!(store.all_active_models().is_empty());

        // 发现新模型
        assert!(store.register_model("gpt-6-astra"));
        assert!(!store.register_model("gpt-6-astra")); // 第二次不是新的
        assert_eq!(store.all_active_models(), vec!["gpt-6-astra"]);

        // 还没 token
        assert!(store.peek_for_model("gpt-6-astra").is_none());
        assert_eq!(store.fresh_count(), 0);
    }

    #[test]
    fn capture_and_peek_for_model() {
        let mut store = TurnStateStore::default();
        let token = token_for(now_unix() - 30);
        assert!(store.capture("gpt-6-astra", &token, "fetch"));
        assert_eq!(
            store.peek_for_model("gpt-6-astra").as_deref(),
            Some(token.as_str())
        );
        assert!(store.peek_for_model("o3-pro").is_none());
        assert_eq!(store.fresh_count(), 1);
    }

    #[test]
    fn capture_replaces_old() {
        let mut store = TurnStateStore::default();
        let old = token_for(now_unix() - 100);
        let new = token_for(now_unix() - 5);
        store.capture("gpt-6-astra", &old, "fetch");
        store.capture("gpt-6-astra", &new, "fetch");
        assert_eq!(
            store.peek_for_model("gpt-6-astra").as_deref(),
            Some(new.as_str())
        );
        assert_eq!(store.fresh_count(), 1);
    }

    #[test]
    fn multi_model_tokens() {
        let mut store = TurnStateStore::default();
        let t1 = token_for(now_unix() - 30);
        let t2 = token_for(now_unix() - 60);
        store.capture("gpt-6-astra", &t1, "fetch");
        store.capture("o3-pro", &t2, "fetch");
        assert_eq!(store.fresh_count(), 2);
        assert_eq!(
            store.peek_for_model("gpt-6-astra").as_deref(),
            Some(t1.as_str())
        );
        assert_eq!(
            store.peek_for_model("o3-pro").as_deref(),
            Some(t2.as_str())
        );
        assert_eq!(store.peek_freshest().as_deref(), Some(t1.as_str()));
    }

    #[test]
    fn needs_refresh_logic() {
        let mut store = TurnStateStore::default();
        assert!(store.needs_refresh("gpt-6-astra"));

        let fresh = token_for(now_unix() - 5);
        store.capture("gpt-6-astra", &fresh, "fetch");
        assert!(!store.needs_refresh("gpt-6-astra"));

        // 满 30 秒 → 再采一张，但仍可注入
        let due = token_for(now_unix() - 30);
        store.capture("gpt-6-astra", &due, "fetch");
        assert!(store.needs_refresh("gpt-6-astra"));
        assert_eq!(store.peek_for_model("gpt-6-astra").as_deref(), Some(due.as_str()));

        // 超过 240 秒 → 停止注入
        let old = token_for(now_unix() - 241);
        store.capture("gpt-6-astra", &old, "fetch");
        assert!(store.needs_refresh("gpt-6-astra"));
        assert!(store.peek_for_model("gpt-6-astra").is_none());

        store.set_lifetime(20 * 60, 10 * 60);
        let mid = token_for(now_unix() - 15 * 60);
        store.capture("gpt-6-astra", &mid, "fetch");
        assert!(store.needs_refresh("gpt-6-astra"));
        assert_eq!(store.peek_for_model("gpt-6-astra").as_deref(), Some(mid.as_str()));
        store.set_lifetime(12 * 60, 8 * 60);
        assert!(store.peek_for_model("gpt-6-astra").is_none());
    }

    #[test]
    fn expired_token_not_returned() {
        let mut store = TurnStateStore::default();
        let expired = token_for(now_unix() - 2460);
        store.capture("gpt-6-astra", &expired, "fetch");
        assert!(store.peek_for_model("gpt-6-astra").is_none());
        assert!(store.peek_freshest().is_none());
    }

    #[test]
    fn invalidate_clears_tokens_not_tracking() {
        let mut store = TurnStateStore::default();
        let token = token_for(now_unix() - 30);
        store.register_model("gpt-6-astra");
        store.register_model("o3-pro");
        store.capture("gpt-6-astra", &token, "fetch");
        store.capture("o3-pro", &token, "fetch");
        assert_eq!(store.fresh_count(), 2);

        store.invalidate_all();
        assert_eq!(store.fresh_count(), 0);
        // 追踪不被清除 → 会立即重新获取
        assert_eq!(store.all_active_models().len(), 2);
    }

    #[test]
    fn invalidate_model_only() {
        let mut store = TurnStateStore::default();
        let token = token_for(now_unix() - 30);
        store.capture("gpt-6-astra", &token, "fetch");
        store.capture("o3-pro", &token, "fetch");
        store.invalidate_model("gpt-6-astra");
        assert!(store.peek_for_model("gpt-6-astra").is_none());
        assert!(store.peek_for_model("o3-pro").is_some());
    }

    #[test]
    fn bind_account_clears_leftovers_on_switch() {
        let mut store = TurnStateStore::default();
        let token = token_for(now_unix() - 30);
        store.register_model("gpt-6-astra");
        store.capture("gpt-6-astra", &token, "fetch");
        store.record_distribution(
            "gpt-6-astra",
            vec![TokenLenCount { len: 292, count: 1 }],
        );

        assert!(store.bind_account("acct-a"));
        assert_eq!(store.fresh_count(), 0);
        assert!(store.all_active_models().is_empty());
        assert!(store.get_distribution("gpt-6-astra").is_empty());
        assert!(!store.bind_account("acct-a"));

        store.register_model("gpt-6-astra");
        store.capture("gpt-6-astra", &token, "fetch");
        assert!(store.bind_account("acct-b"));
        assert_eq!(store.fresh_count(), 0);
        assert!(store.all_active_models().is_empty());
        assert_eq!(store.bound_len(), QUALITY_TOKEN_LEN);
        assert!(store.auto_quality_len.is_none());
    }

    #[test]
    fn rejects_312_token() {
        let mut store = TurnStateStore::default();
        let degraded = "a".repeat(312);
        assert!(!store.capture("gpt-6-astra", &degraded, "fetch"));
        assert!(store.peek_for_model("gpt-6-astra").is_none());
    }

    #[test]
    fn detects_degraded_token_length() {
        let short = "a".repeat(292);
        assert!(is_quality_token(&short));
        assert!(!is_degraded_token(&short));
        let long = "a".repeat(312);
        assert!(is_degraded_token(&long));
        assert!(!is_quality_token(&long));
        let alt = "a".repeat(332);
        assert!(is_quality_token(&alt));
        assert!(!is_degraded_token(&alt));
        assert_eq!(token_quality_label(&alt), "332/normal");
        assert_eq!(canonical_quality_len(332), Some(QUALITY_TOKEN_LEN_332));
    }

    #[test]
    fn auto_default_follows_292_or_332() {
        let mut store = TurnStateStore::default();
        assert_eq!(store.bound_len(), QUALITY_TOKEN_LEN);

        let t332 = token_for_len(now_unix() - 10, QUALITY_TOKEN_LEN_332);
        assert!(canonical_quality_len(t332.trim().len()) == Some(QUALITY_TOKEN_LEN_332));
        assert!(store.capture("gpt-6-astra", &t332, "fetch"));
        assert_eq!(store.bound_len(), QUALITY_TOKEN_LEN_332);
        assert!(store.peek_for_model("gpt-6-astra").is_some());

        let mut other = TurnStateStore::default();
        let t292 = token_for(now_unix() - 10);
        assert!(other.capture("gpt-6-astra", &t292, "fetch"));
        assert_eq!(other.bound_len(), QUALITY_TOKEN_LEN);
        assert!(other.peek_for_model("gpt-6-astra").is_some());
    }

    #[test]
    fn view_shows_discovered_models() {
        let mut store = TurnStateStore::default();
        store.register_model("gpt-6-astra");
        store.register_model("o3-pro");
        let token = token_for(now_unix() - 10);
        store.capture("gpt-6-astra", &token, "fetch");

        let view = store.view();
        assert_eq!(view.status, "partial"); // one active, one empty
        assert_eq!(view.models.len(), 2);

        let astra = view.models.iter().find(|m| m.model == "gpt-6-astra").unwrap();
        assert_eq!(astra.status, "active");
        let o3 = view.models.iter().find(|m| m.model == "o3-pro").unwrap();
        assert_eq!(o3.status, "empty");
    }

    #[test]
    fn view_idle_when_no_models() {
        let store = TurnStateStore::default();
        let view = store.view();
        assert_eq!(view.status, "idle");
        assert!(view.models.is_empty());
    }

    #[test]
    fn stamps_ws_client_metadata() {
        let token = token_for(now_unix());
        let out = stamp_ws_json(r#"{"type":"item","client_metadata":{}}"#, &token).unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["client_metadata"][HEADER_NAME], json!(token));
    }

    #[test]
    fn inserts_ws_token_when_missing() {
        let token = token_for(now_unix());
        let out = stamp_ws_json(r#"{"foo":1}"#, &token).unwrap();
        let value: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["client_metadata"][HEADER_NAME], json!(token));
    }

    #[test]
    fn leaves_non_json_ws_alone() {
        assert!(stamp_ws_json("not-json", "gAAAAA").is_none());
    }

    #[test]
    fn finds_nested_token() {
        let token = token_for(now_unix());
        let payload = json!({
            "event": "x",
            "headers": { "X-Codex-Turn-State": token }
        });
        assert_eq!(
            token_from_json(&payload.to_string()).as_deref(),
            Some(token.as_str())
        );
    }

    #[test]
    fn http_follow_up_has_turn_state() {
        let mut headers = HeaderMap::new();
        assert!(!has_http_turn_state(&headers));
        headers.insert(
            HeaderName::from_static(HEADER_NAME),
            HeaderValue::from_static("ts-1"),
        );
        assert!(has_http_turn_state(&headers));
        clear_http_header(&mut headers);
        assert!(!has_http_turn_state(&headers));
    }

    #[test]
    fn http_stamp_overwrites_header() {
        let token = token_for(now_unix());
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(HEADER_NAME),
            HeaderValue::from_static("old"),
        );
        apply_http_header(&mut headers, &token);
        assert_eq!(header_token(&headers).as_deref(), Some(token.as_str()));
        assert!(should_stamp_http("POST", "/backend-api/codex/responses"));
        assert!(!should_stamp_http("GET", "/responses"));
        assert!(is_context_compaction("/responses/compact", b"{}"));
        assert!(is_context_compaction(
            "/responses",
            br#"{"model":"gpt-6-astra","compaction_trigger":"auto"}"#,
        ));
        assert!(!is_context_compaction(
            "/responses",
            br#"{"model":"gpt-6-astra"}"#,
        ));
        assert!(injectable_http_token(&token).is_some());
        assert!(injectable_http_token("client-state").is_none());
        assert!(injectable_http_token(&token_for_len(now_unix(), DEGRADED_TOKEN_LEN)).is_none());
        apply_http_header(&mut headers, "not-a-ticket");
        assert_eq!(header_token(&headers).as_deref(), Some(token.as_str()));
    }

    #[test]
    fn same_turn_follow_up_uses_previous_response_or_trailing_tool_output() {
        assert!(!is_same_turn_follow_up(br#"{"model":"gpt-6-astra"}"#));
        assert!(!is_same_turn_follow_up(
            br#"{"model":"gpt-6-astra","input":[{"role":"user","content":"hi"}]}"#
        ));
        assert!(!is_same_turn_follow_up(br#"{"model":"gpt-6-astra","input":[
            {"type":"function_call_output","call_id":"c1","output":"old"},
            {"role":"user","content":"next turn"}
        ]}"#));
        assert!(is_same_turn_follow_up(
            br#"{"model":"gpt-6-astra","previous_response_id":"resp_1"}"#
        ));
        assert!(is_same_turn_follow_up(br#"{"model":"gpt-6-astra","input":[
            {"role":"user","content":"hi"},
            {"type":"function_call_output","call_id":"c1","output":"ok"}
        ]}"#));
        assert!(has_body_turn_state(
            br#"{"client_metadata":{"x-codex-turn-state":"gAAAAAexample"}}"#
        ));
        assert!(!has_body_turn_state(br#"{"client_metadata":{}}"#));
    }

    #[test]
    fn wrap_second_packet_stamps_metadata_and_strips_previous_response_id() {
        let token = token_for(now_unix());
        let wrapped = wrap_as_second_packet(
            br#"{"model":"gpt-6-astra","input":[{"role":"user","content":"hi"}]}"#,
            None,
            &token,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&wrapped).unwrap();
        assert!(value.get("previous_response_id").is_none());
        assert_eq!(
            value
                .pointer(&format!("/client_metadata/{HEADER_NAME}"))
                .and_then(Value::as_str),
            Some(token.as_str())
        );

        let stripped = wrap_as_second_packet(
            br#"{"model":"gpt-6-astra","previous_response_id":"resp_client"}"#,
            None,
            &token,
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&stripped).unwrap();
        assert!(value.get("previous_response_id").is_none());
        assert_eq!(
            value
                .pointer(&format!("/client_metadata/{HEADER_NAME}"))
                .and_then(Value::as_str),
            Some(token.as_str())
        );
    }

    #[test]
    fn extract_model_from_request() {
        let body = br#"{"model":"gpt-6-astra","input":[{"type":"message"}]}"#;
        assert_eq!(
            extract_model_from_body(body).as_deref(),
            Some("gpt-6-astra")
        );
        assert!(extract_model_from_body(b"not-json").is_none());
        assert!(extract_model_from_body(b"{}").is_none());
    }

    #[test]
    fn rewrite_model_inserts_and_replaces_json() {
        let replaced = rewrite_model_in_body(
            br#"{"model":"gpt-5.4","stream":true}"#,
            None,
            "gpt-6-astra",
        )
        .unwrap();
        assert_eq!(
            extract_model_from_body(&replaced).as_deref(),
            Some("gpt-6-astra")
        );
        let inserted = rewrite_model_in_body(br#"{"stream":true}"#, None, "gpt-6-astra").unwrap();
        assert_eq!(
            extract_model_from_body(&inserted).as_deref(),
            Some("gpt-6-astra")
        );
        assert!(rewrite_model_in_body(b"not-json", None, "gpt-6-astra").is_err());
    }

    #[test]
    fn rewrite_model_round_trips_gzip_and_zstd() {
        let json = br#"{"model":"other","input":[]}"#;
        let gzip = {
            use std::io::Write;
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(json).unwrap();
            encoder.finish().unwrap()
        };
        let rewritten = rewrite_model_in_body(&gzip, Some("gzip"), "gpt-6-astra").unwrap();
        assert_eq!(
            extract_model_from_body(&rewritten).as_deref(),
            Some("gpt-6-astra")
        );
        let zstd = zstd::encode_all(&json[..], 0).unwrap();
        let rewritten = rewrite_model_in_body(&zstd, Some("zstd"), "gpt-6-astra").unwrap();
        assert_eq!(
            extract_model_from_body(&rewritten).as_deref(),
            Some("gpt-6-astra")
        );
    }

    #[test]
    fn persistence_round_trip() {
        let mut store = TurnStateStore::default();
        store.register_model("gpt-6-astra");
        store.register_model("o3-pro");
        let token = token_for(now_unix() - 30);
        store.capture("gpt-6-astra", &token, "fetch");

        let persisted = PersistedStore {
            version: PERSIST_VERSION,
            tokens: store.tokens.clone(),
            active_models: store.active_models.clone(),
            bound_token_len: store.bound_token_len,
            auto_quality_len: store.auto_quality_len,
            model_bound_lens: store.model_bound_lens.clone(),
            pool: store.pool.clone(),
            distributions: store.distributions.clone(),
            account_id: store.account_id.clone(),
        };
        let json = serde_json::to_string(&persisted).unwrap();
        let restored: PersistedStore = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.version, PERSIST_VERSION);
        assert_eq!(restored.tokens.len(), 1);
        assert_eq!(restored.active_models.len(), 2);
    }

    #[test]
    fn capture_binds_and_restores_proxy_session() {
        let mut store = TurnStateStore::default();
        let token = token_for(now_unix() - 10);
        let cookies = vec![crate::chatgpt_cookies::RoutingCookie {
            name: "__oailb".into(),
            value: "route1".into(),
            expires_unix: Some(now_unix() + 3600),
        }];
        assert!(store.capture_with_session(
            "m1",
            &token,
            "fetch",
            Some("1Z5jzVPs"),
            Some("resp_probe"),
            &cookies
        ));
        assert_eq!(
            store.previous_response_id_for_token(Some(&token)).as_deref(),
            Some("resp_probe")
        );
        let cookie_names = store
            .routing_cookies_for_token(Some(&token))
            .into_iter()
            .map(|cookie| cookie.name)
            .collect::<Vec<_>>();
        assert_eq!(cookie_names, vec!["__oailb"]);
        assert_eq!(store.bound_routing_cookies()[0].name, "__oailb");
        assert_eq!(store.bound_proxy_session().as_deref(), Some("1Z5jzVPs"));
        assert_eq!(
            store.proxy_session_for_token(Some(&token)).as_deref(),
            Some("1Z5jzVPs")
        );
        assert_eq!(store.view().bound_proxy_session.as_deref(), Some("1Z5jzVPs"));

        let persisted = serde_json::to_string(&PersistedStore {
            version: PERSIST_VERSION,
            tokens: store.tokens.clone(),
            active_models: store.active_models.clone(),
            bound_token_len: store.bound_token_len,
            auto_quality_len: store.auto_quality_len,
            model_bound_lens: store.model_bound_lens.clone(),
            pool: store.pool.clone(),
            distributions: store.distributions.clone(),
            account_id: store.account_id.clone(),
        })
        .unwrap();
        let restored: PersistedStore = serde_json::from_str(&persisted).unwrap();
        assert_eq!(
            restored.tokens.get("m1").map(|ts| ts.proxy_session.as_str()),
            Some("1Z5jzVPs")
        );
        assert_eq!(
            restored
                .tokens
                .get("m1")
                .map(|ts| ts.previous_response_id.as_str()),
            Some("resp_probe")
        );
        assert_eq!(
            restored
                .tokens
                .get("m1")
                .map(|ts| ts.routing_cookies.iter().map(|cookie| cookie.name.as_str()).collect::<Vec<_>>()),
            Some(vec!["__oailb"])
        );
        let legacy: TurnState = serde_json::from_value(serde_json::json!({
            "token": token,
            "issued_unix": now_unix() - 10,
            "len": token.len(),
            "source": "fetch",
            "captured_at": ""
        }))
        .unwrap();
        assert!(legacy.proxy_session.is_empty());
        assert!(legacy.previous_response_id.is_empty());
        assert!(legacy.routing_cookies.is_empty());
    }

    #[test]
    fn capture_drops_session_cookies_and_expired_routing_cookies() {
        let mut store = TurnStateStore::default();
        let token = token_for(now_unix() - 10);
        assert!(store.capture_with_session(
            "m1",
            &token,
            "fetch",
            None,
            None,
            &[
                crate::chatgpt_cookies::RoutingCookie {
                    name: "__Secure-next-auth.session-token".into(),
                    value: "stolen".into(),
                    expires_unix: None,
                },
                crate::chatgpt_cookies::RoutingCookie {
                    name: "chatgpt_session".into(),
                    value: "nope".into(),
                    expires_unix: None,
                },
                crate::chatgpt_cookies::RoutingCookie {
                    name: "__cflb".into(),
                    value: "dead".into(),
                    expires_unix: Some(now_unix() - 1),
                },
                crate::chatgpt_cookies::RoutingCookie {
                    name: "__oailb".into(),
                    value: "live".into(),
                    expires_unix: Some(now_unix() + 3600),
                },
            ],
        ));
        let names = store
            .routing_cookies_for_token(Some(&token))
            .into_iter()
            .map(|cookie| cookie.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["__oailb"]);
    }

    #[test]
    fn bound_len_defaults_to_292() {
        let store = TurnStateStore::default();
        assert_eq!(store.bound_len(), QUALITY_TOKEN_LEN);
    }

    #[test]
    fn bound_len_filters_capture() {
        let mut store = TurnStateStore::default();
        // 默认绑定 292
        let t292 = token_for(now_unix() - 10);
        assert!(store.capture("m1", &t292, "fetch"));
        // 292 匹配绑定 → peek 返回 token
        assert!(store.peek_for_model("m1").is_some());

        // 323 长度的 token 也能入池（不再拒绝），但 peek 不返回（不匹配绑定）
        let t323 = token_for_len(now_unix() - 10, 323);
        assert!(store.capture("m2", &t323, "fetch")); // 入池成功
        assert!(store.peek_for_model("m2").is_none()); // 但 peek 不返回（绑定是 292）
        // 池中应该有这个 token
        let pool_m2 = store.pool_summary("m2");
        assert_eq!(pool_m2.len(), 1);
        let actual_len_323 = t323.trim().len();
        assert_eq!(pool_m2[0].0, actual_len_323); // 实际 token 长度
        assert!(!pool_m2[0].1);                    // is_bound = false

        // 切换绑定到 323 → 从池中自动提升，无需重新获取
        store.set_bound_len(Some(323));
        assert_eq!(store.bound_len(), 323);
        assert!(store.peek_for_model("m2").is_some()); // 自动从池中提升了

        // 旧 292 token 仍在池中，但 peek 不返回（绑定已改）
        assert!(store.peek_for_model("m1").is_none());
        let pool_m1 = store.pool_summary("m1");
        assert_eq!(pool_m1.len(), 1);
        assert_eq!(pool_m1[0].0, t292.trim().len()); // 仍然在池中

        // 切回 292 → m1 又可以 peek 了
        store.set_bound_len(None);
        assert!(store.peek_for_model("m1").is_some());
    }

    #[test]
    fn extract_model_from_gzip_body() {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;

        let json = br#"{"model":"gpt-6-astra","input":[{"type":"message"}],"stream":true}"#;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(json).unwrap();
        let compressed = encoder.finish().unwrap();

        // 压缩后不是有效 JSON
        assert!(serde_json::from_slice::<Value>(&compressed).is_err());
        // 但 extract_model_from_body 应该能自动解压并提取
        assert_eq!(
            extract_model_from_body(&compressed).as_deref(),
            Some("gpt-6-astra")
        );
    }

    #[test]
    fn extract_model_from_zstd_body() {
        let json = br#"{"model":"gpt-6-astra","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}],"stream":true}"#;
        let compressed = zstd::encode_all(std::io::Cursor::new(json), 3).unwrap();

        // 验证 zstd magic
        assert_eq!(&compressed[..4], &[0x28, 0xB5, 0x2F, 0xFD]);
        // extract 应该成功
        assert_eq!(
            extract_model_from_body(&compressed).as_deref(),
            Some("gpt-6-astra")
        );
    }

    #[test]
    fn extract_model_from_deflate_body() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let json = br#"{"model":"o3-pro","input":[]}"#;
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(json).unwrap();
        let compressed = encoder.finish().unwrap();

        assert_eq!(
            extract_model_from_body(&compressed).as_deref(),
            Some("o3-pro")
        );
    }

    #[test]
    fn distribution_recording() {
        let mut store = TurnStateStore::default();
        assert!(store.get_distribution("m1").is_empty());

        store.record_distribution(
            "m1",
            vec![
                TokenLenCount { len: 292, count: 3 },
                TokenLenCount { len: 323, count: 7 },
            ],
        );
        let dist = store.get_distribution("m1");
        assert_eq!(dist.len(), 2);
        assert_eq!(dist[0].len, 292);
        assert_eq!(dist[0].count, 3);
        assert_eq!(dist[1].len, 323);
        assert_eq!(dist[1].count, 7);
    }
}
