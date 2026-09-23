//! Durable per-upstream-account usage and cost accounting.
//!
//! This module deliberately keeps billing separate from the bounded in-memory
//! network log.  A request is inserted as `pending` before it is sent upstream
//! and settled at most once by its UUID.  Missing provider usage is represented
//! explicitly instead of being converted to zero cost.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, ToSql};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::downgrade::{DowngradeReport, DowngradeSignals, Verdict};
use crate::pricing::{PriceBook, ServiceTier, Usage as PricingUsage};

const PROVIDER_CHATGPT: &str = "chatgpt";

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UsageState {
    #[default]
    Pending,
    Measured,
    MissingUsage,
    Interrupted,
}

impl UsageState {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Measured => "measured",
            Self::MissingUsage => "missing_usage",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "measured" => Self::Measured,
            "missing_usage" => Self::MissingUsage,
            "interrupted" => Self::Interrupted,
            _ => Self::Pending,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    #[serde(default)]
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    /// Informational: already included in `output_tokens`.
    #[serde(default)]
    pub reasoning_tokens: Option<u64>,
}

impl TokenUsage {
    pub fn complete(&self) -> bool {
        self.input_tokens.is_some() && self.output_tokens.is_some()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestStart {
    pub request_id: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    pub account_id: String,
    pub email: Option<String>,
    pub source: String,
    pub started_at: String,
    pub requested_model: Option<String>,
    pub sent_model: Option<String>,
    /// `service_tier` from the request body.
    #[serde(default)]
    pub service_tier: Option<String>,
}

fn default_provider() -> String {
    PROVIDER_CHATGPT.into()
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageOutcome {
    pub state: UsageState,
    pub finished_at: Option<String>,
    pub http_status: Option<u16>,
    pub response_model: Option<String>,
    pub usage: TokenUsage,
    pub usage_source: Option<String>,
    pub error_kind: Option<String>,
    /// `service_tier` reported by the response.
    #[serde(default)]
    pub service_tier: Option<String>,
    /// Time to the first visible output, kept so latency survives restarts.
    #[serde(default)]
    pub first_token_ms: Option<u64>,
    /// `http`, `http_sse`, `http_to_ws` or `ws_to_ws`.
    #[serde(default)]
    pub transport: Option<String>,
    /// Downgrade evidence gathered from the response head and stream.
    #[serde(default)]
    pub downgrade_signals: DowngradeSignals,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingRuleSpec {
    pub provider: String,
    pub model: String,
    /// Price in nano-dollars per million tokens.
    pub input_nanos_per_million: i64,
    pub cached_input_nanos_per_million: i64,
    pub output_nanos_per_million: i64,
    #[serde(default = "default_currency")]
    pub currency: String,
    #[serde(default)]
    pub effective_from: Option<String>,
}

fn default_currency() -> String {
    "USD".into()
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    pub request_id: String,
    pub provider: String,
    pub account_id: String,
    pub email: Option<String>,
    pub source: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub state: UsageState,
    pub http_status: Option<u16>,
    pub requested_model: Option<String>,
    pub sent_model: Option<String>,
    pub response_model: Option<String>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
    pub usage_source: Option<String>,
    pub pricing_rule_id: Option<i64>,
    /// Catalog key (or manual rule model) the cost was priced with.
    pub pricing_model: Option<String>,
    /// Billed service tier: `standard`, `priority` or `flex`.
    pub service_tier: Option<String>,
    pub long_context: bool,
    pub input_cost_nanos: Option<i64>,
    pub cache_read_cost_nanos: Option<i64>,
    pub cache_write_cost_nanos: Option<i64>,
    pub output_cost_nanos: Option<i64>,
    pub first_token_ms: Option<u64>,
    pub transport: Option<String>,
    /// Set when the response looks downgraded (see [`crate::downgrade`]).
    pub downgrade: Option<DowngradeReport>,
    pub cost_nanos: Option<i64>,
    pub currency: Option<String>,
    pub error_kind: Option<String>,
}

/// Columns read by [`row_to_record`], in order.
const RECORD_COLUMNS: &str = "u.request_id,a.provider,a.upstream_account_id,a.display_email,u.source,u.started_at,u.finished_at,u.state,u.http_status,u.requested_model,u.sent_model,u.response_model,u.input_tokens,u.cached_input_tokens,u.output_tokens,u.usage_source,u.pricing_rule_id,u.cost_nanos,u.currency,u.error_kind,u.cache_write_tokens,u.reasoning_tokens,u.pricing_model,u.service_tier,u.long_context,u.input_cost_nanos,u.cache_read_cost_nanos,u.cache_write_cost_nanos,u.output_cost_nanos,u.first_token_ms,u.transport,u.downgrade";

/// (state, source, provider, sent_model, started_at, requested_service_tier)
type PendingRow = (
    String,
    String,
    String,
    Option<String>,
    String,
    Option<String>,
);

#[derive(Default)]
struct Priced {
    rule_id: Option<i64>,
    model: Option<String>,
    tier: Option<ServiceTier>,
    long_context: bool,
    input: Option<i64>,
    cache_read: Option<i64>,
    cache_write: Option<i64>,
    output: Option<i64>,
    total: Option<i64>,
    currency: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageFilter {
    pub account_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
    /// `Some(true)` keeps only downgraded (confirmed or suspected) requests.
    #[serde(default)]
    pub downgraded: Option<bool>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub request_count: u64,
    pub measured_request_count: u64,
    pub unknown_usage_count: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
    pub cost_nanos: Option<i64>,
    // Kept out of the wire format.  A summary must remain NULL when any row
    // in that bucket has unknown usage or no matching price rule.
    #[serde(skip)]
    cost_complete: bool,
}

impl Default for UsageTotals {
    fn default() -> Self {
        Self {
            request_count: 0,
            measured_request_count: 0,
            unknown_usage_count: 0,
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            reasoning_tokens: 0,
            cost_nanos: None,
            cost_complete: true,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    pub provider: String,
    pub account_id: String,
    pub email: Option<String>,
    pub first_seen_at: String,
    pub last_seen_at: String,
    pub total: UsageTotals,
    pub business: UsageTotals,
    pub internal: UsageTotals,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BillingSummary {
    pub generated_at: String,
    pub from: Option<String>,
    pub to: Option<String>,
    pub accounts: Vec<AccountSummary>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecordsPage {
    pub records: Vec<UsageRecord>,
    pub total: u64,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Clone)]
pub struct BillingStore {
    connection: Arc<Mutex<Connection>>,
    pricing: Arc<PriceBook>,
    /// Latest downgraded request settled since Kit started.
    last_downgrade: Arc<Mutex<Option<DowngradeEvent>>>,
    /// Bumped on every write, so the UI can poll cheaply and reload only
    /// when something changed.
    revision: Arc<AtomicU64>,
}

/// A downgraded request, surfaced to the UI as a notice.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DowngradeEvent {
    pub request_id: String,
    pub at: String,
    pub account_id: String,
    pub email: Option<String>,
    pub report: DowngradeReport,
}

impl BillingStore {
    pub fn open(path: impl AsRef<Path>, pricing: Arc<PriceBook>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create billing directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("open billing database {}", path.display()))?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
            pricing,
            last_downgrade: Arc::new(Mutex::new(None)),
            revision: Arc::default(),
        };
        store.configure()?;
        store.migrate()?;
        store.recover_pending()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            connection: Arc::new(Mutex::new(Connection::open_in_memory()?)),
            pricing: Arc::new(PriceBook::bundled()),
            last_downgrade: Arc::new(Mutex::new(None)),
            revision: Arc::default(),
        };
        store.configure()?;
        store.migrate()?;
        Ok(store)
    }

    pub fn pricing(&self) -> &Arc<PriceBook> {
        &self.pricing
    }

    /// Changes whenever a record or price rule is written.
    pub fn revision(&self) -> u64 {
        self.revision.load(Ordering::SeqCst)
    }

    fn bump_revision(&self) {
        self.revision.fetch_add(1, Ordering::SeqCst);
    }

    pub fn last_downgrade(&self) -> Option<DowngradeEvent> {
        self.last_downgrade
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn configure(&self) -> Result<()> {
        self.connection()
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
        Ok(())
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.connection();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY);\
             CREATE TABLE IF NOT EXISTS accounts (\
               id INTEGER PRIMARY KEY, provider TEXT NOT NULL, upstream_account_id TEXT NOT NULL,\
               display_email TEXT, first_seen_at TEXT NOT NULL, last_seen_at TEXT NOT NULL,\
               UNIQUE(provider, upstream_account_id)\
             );\
             CREATE TABLE IF NOT EXISTS pricing_rules (\
               id INTEGER PRIMARY KEY, provider TEXT NOT NULL, model TEXT NOT NULL,\
               input_nanos_per_million INTEGER NOT NULL, cached_input_nanos_per_million INTEGER NOT NULL,\
               output_nanos_per_million INTEGER NOT NULL, currency TEXT NOT NULL, effective_from TEXT NOT NULL,\
               UNIQUE(provider, model, effective_from)\
             );\
             CREATE TABLE IF NOT EXISTS usage_records (\
               request_id TEXT PRIMARY KEY, account_id INTEGER NOT NULL REFERENCES accounts(id),\
               source TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT, state TEXT NOT NULL,\
               http_status INTEGER, requested_model TEXT, sent_model TEXT, response_model TEXT,\
               input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, usage_source TEXT,\
               pricing_rule_id INTEGER REFERENCES pricing_rules(id), cost_nanos INTEGER, currency TEXT, error_kind TEXT\
             );\
             CREATE INDEX IF NOT EXISTS usage_by_account_time ON usage_records(account_id, started_at);\
             CREATE INDEX IF NOT EXISTS usage_by_state_time ON usage_records(state, started_at);\
             CREATE INDEX IF NOT EXISTS usage_by_source_time ON usage_records(source, started_at);",
        )?;
        // Version 1 is intentionally idempotent so databases created by the
        // first MVP (which had the marker table but no row) are upgraded too.
        conn.execute(
            "INSERT OR IGNORE INTO schema_migrations(version) VALUES (1)",
            [],
        )?;
        // Version 2: cache writes, reasoning tokens, service tier and a
        // per-bucket cost breakdown (sub2api's CostBreakdown).
        let existing: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('usage_records')")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for (column, kind) in [
            ("cache_write_tokens", "INTEGER"),
            ("reasoning_tokens", "INTEGER"),
            ("requested_service_tier", "TEXT"),
            ("service_tier", "TEXT"),
            ("pricing_model", "TEXT"),
            ("long_context", "INTEGER NOT NULL DEFAULT 0"),
            ("input_cost_nanos", "INTEGER"),
            ("cache_read_cost_nanos", "INTEGER"),
            ("cache_write_cost_nanos", "INTEGER"),
            ("output_cost_nanos", "INTEGER"),
            ("first_token_ms", "INTEGER"),
            ("transport", "TEXT"),
            // JSON DowngradeReport, and its verdict for filtering.
            ("downgrade", "TEXT"),
            ("downgrade_verdict", "TEXT"),
        ] {
            if !existing.iter().any(|name| name == column) {
                conn.execute_batch(&format!(
                    "ALTER TABLE usage_records ADD COLUMN {column} {kind}"
                ))?;
            }
        }
        conn.execute(
            "INSERT OR IGNORE INTO schema_migrations(version) VALUES (2)",
            [],
        )?;
        Ok(())
    }

    /// Pending rows survive crashes; mark them as interrupted on the next boot.
    pub fn recover_pending(&self) -> Result<u64> {
        let changed = self.connection().execute(
            "UPDATE usage_records SET state='interrupted', finished_at=COALESCE(finished_at, strftime('%Y-%m-%dT%H:%M:%fZ','now')), error_kind=COALESCE(error_kind,'process_restarted') WHERE state='pending'",
            [],
        )?;
        Ok(changed as u64)
    }

    pub fn ensure_account(
        &self,
        provider: &str,
        account_id: &str,
        email: Option<&str>,
        seen_at: &str,
    ) -> Result<i64> {
        let provider = if provider.trim().is_empty() {
            PROVIDER_CHATGPT
        } else {
            provider.trim()
        };
        let account_id = account_id.trim();
        anyhow::ensure!(!account_id.is_empty(), "billing account id is empty");
        let conn = self.connection();
        conn.execute(
            "INSERT INTO accounts(provider, upstream_account_id, display_email, first_seen_at, last_seen_at) VALUES (?1,?2,?3,?4,?4) ON CONFLICT(provider, upstream_account_id) DO UPDATE SET display_email=COALESCE(excluded.display_email, accounts.display_email), last_seen_at=excluded.last_seen_at",
            params![provider, account_id, email, seen_at],
        )?;
        Ok(conn.query_row(
            "SELECT id FROM accounts WHERE provider=?1 AND upstream_account_id=?2",
            params![provider, account_id],
            |row| row.get(0),
        )?)
    }

    pub fn begin_request(&self, start: RequestStart) -> Result<UsageRecord> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let account_db_id = {
            tx.execute(
                "INSERT INTO accounts(provider, upstream_account_id, display_email, first_seen_at, last_seen_at) VALUES (?1,?2,?3,?4,?4) ON CONFLICT(provider, upstream_account_id) DO UPDATE SET display_email=COALESCE(excluded.display_email, accounts.display_email), last_seen_at=excluded.last_seen_at",
                params![start.provider, start.account_id, start.email, start.started_at],
            )?;
            tx.query_row(
                "SELECT id FROM accounts WHERE provider=?1 AND upstream_account_id=?2",
                params![start.provider, start.account_id],
                |row| row.get::<_, i64>(0),
            )?
        };
        tx.execute(
            "INSERT OR IGNORE INTO usage_records(request_id, account_id, source, started_at, state, requested_model, sent_model, requested_service_tier) VALUES (?1,?2,?3,?4,'pending',?5,?6,?7)",
            params![start.request_id, account_db_id, start.source, start.started_at, start.requested_model, start.sent_model, start.service_tier],
        )?;
        tx.commit()?;
        drop(conn);
        self.bump_revision();
        self.get_by_id(&start.request_id)?
            .context("billing insert did not produce a record")
    }

    pub fn settle_request(&self, request_id: &str, outcome: UsageOutcome) -> Result<UsageRecord> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let existing: Option<PendingRow> = tx
            .query_row("SELECT u.state, u.source, a.provider, u.sent_model, u.started_at, u.requested_service_tier FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE u.request_id=?1", params![request_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))
            .optional()?;
        let Some((old_state, source, provider, sent_model, started_at, requested_tier)) = existing
        else {
            anyhow::bail!("unknown billing request id {request_id}");
        };
        if old_state != "pending" {
            drop(tx);
            drop(conn);
            return self
                .get_by_id(request_id)?
                .context("settled billing record disappeared");
        }
        let measured = outcome.state == UsageState::Measured && outcome.usage.complete();
        let state = if measured {
            UsageState::Measured
        } else {
            outcome.state
        };
        let priced = if measured {
            // Like sub2api, bill the model that was sent upstream; the
            // response's model name is only a fallback.
            let models = [sent_model.as_deref(), outcome.response_model.as_deref()];
            let tier =
                ServiceTier::billed(requested_tier.as_deref(), outcome.service_tier.as_deref());
            self.resolve_cost(
                &tx,
                &provider,
                &source,
                &models,
                &started_at,
                &outcome.usage,
                tier,
            )?
        } else {
            Priced::default()
        };
        let tokens = |value: Option<u64>| value.map(|v| v as i64);
        let downgrade = outcome
            .downgrade_signals
            .report(sent_model.as_deref(), outcome.response_model.as_deref());
        let downgrade_json = downgrade.as_ref().map(serde_json::to_string).transpose()?;
        let downgrade_verdict = downgrade.as_ref().map(|report| match report.verdict {
            Verdict::Confirmed => "confirmed",
            Verdict::Suspected => "suspected",
        });
        tx.execute(
            "UPDATE usage_records SET finished_at=?2, state=?3, http_status=?4, response_model=?5, input_tokens=?6, cached_input_tokens=?7, output_tokens=?8, usage_source=?9, pricing_rule_id=?10, cost_nanos=?11, currency=?12, error_kind=?13, cache_write_tokens=?14, reasoning_tokens=?15, pricing_model=?16, service_tier=?17, long_context=?18, input_cost_nanos=?19, cache_read_cost_nanos=?20, cache_write_cost_nanos=?21, output_cost_nanos=?22, first_token_ms=?23, transport=?24, downgrade=?25, downgrade_verdict=?26 WHERE request_id=?1",
            params![request_id, outcome.finished_at, state.as_str(), outcome.http_status.map(i64::from), outcome.response_model, tokens(outcome.usage.input_tokens), tokens(outcome.usage.cached_input_tokens), tokens(outcome.usage.output_tokens), outcome.usage_source, priced.rule_id, priced.total, priced.currency, outcome.error_kind, tokens(outcome.usage.cache_write_tokens), tokens(outcome.usage.reasoning_tokens), priced.model, priced.tier.map(ServiceTier::as_str), priced.long_context, priced.input, priced.cache_read, priced.cache_write, priced.output, tokens(outcome.first_token_ms), outcome.transport, downgrade_json, downgrade_verdict],
        )?;
        tx.commit()?;
        drop(conn);
        self.bump_revision();
        let record = self
            .get_by_id(request_id)?
            .context("settled billing record disappeared")?;
        if let Some(report) = record.downgrade.clone() {
            *self
                .last_downgrade
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(DowngradeEvent {
                request_id: record.request_id.clone(),
                at: record
                    .finished_at
                    .clone()
                    .unwrap_or_else(|| record.started_at.clone()),
                account_id: record.account_id.clone(),
                email: record.email.clone(),
                report,
            });
        }
        Ok(record)
    }

    /// Manual pricing rules (exact model match) take precedence, like
    /// sub2api's override file; otherwise the live price catalog is used.
    #[allow(clippy::too_many_arguments)]
    fn resolve_cost(
        &self,
        tx: &rusqlite::Transaction<'_>,
        provider: &str,
        source: &str,
        models: &[Option<&str>],
        effective_at: &str,
        usage: &TokenUsage,
        tier: ServiceTier,
    ) -> Result<Priced> {
        if source != "business" {
            return Ok(Priced::default());
        }
        let models: Vec<&str> = models
            .iter()
            .flatten()
            .map(|m| m.trim())
            .filter(|m| !m.is_empty())
            .collect();
        if models.is_empty() {
            return Ok(Priced::default());
        }
        let usage = PricingUsage {
            input_tokens: usage.input_tokens.unwrap_or(0),
            cache_read_tokens: usage.cached_input_tokens.unwrap_or(0),
            cache_write_tokens: usage.cache_write_tokens.unwrap_or(0),
            output_tokens: usage.output_tokens.unwrap_or(0),
        };
        for model in &models {
            let rule: Option<(i64, i64, i64, String, i64)> = tx
                .query_row("SELECT input_nanos_per_million, cached_input_nanos_per_million, output_nanos_per_million, currency, id FROM pricing_rules WHERE provider=?1 AND model=?2 AND effective_from <= ?3 ORDER BY effective_from DESC, id DESC LIMIT 1", params![provider, model, effective_at], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
                .optional()?;
            if let Some((input_price, cached_price, output_price, currency, id)) = rule {
                // Manual rules have no cache-write or tier prices: cache
                // writes are billed as input and the tier is ignored.
                let cached = usage.cache_read_tokens.min(usage.input_tokens);
                let uncached = usage.input_tokens - cached;
                let cost = |tokens: u64, price: i64| -> Result<i64> {
                    i64::try_from(i128::from(tokens) * i128::from(price) / 1_000_000)
                        .context("billing cost overflow")
                };
                let input = cost(uncached, input_price)?;
                let cache_read = cost(cached, cached_price)?;
                let output = cost(usage.output_tokens, output_price)?;
                return Ok(Priced {
                    rule_id: Some(id),
                    model: Some((*model).to_string()),
                    tier: Some(ServiceTier::Standard),
                    long_context: false,
                    input: Some(input),
                    cache_read: Some(cache_read),
                    cache_write: Some(0),
                    output: Some(output),
                    total: Some(input + cache_read + output),
                    currency: Some(currency),
                });
            }
        }
        let catalog = self.pricing.current();
        let Some(found) = models.iter().find_map(|model| catalog.resolve(model)) else {
            return Ok(Priced::default());
        };
        let cost = found.price.quote(&usage, tier);
        Ok(Priced {
            rule_id: None,
            model: Some(found.key.to_string()),
            tier: Some(tier),
            long_context: cost.long_context,
            input: Some(cost.input),
            cache_read: Some(cost.cache_read),
            cache_write: Some(cost.cache_write),
            output: Some(cost.output),
            total: Some(cost.total),
            currency: Some("USD".into()),
        })
    }

    pub fn mark_missing_usage(
        &self,
        request_id: &str,
        status: Option<u16>,
        error: Option<&str>,
        finished_at: Option<String>,
    ) -> Result<UsageRecord> {
        self.settle_request(
            request_id,
            UsageOutcome {
                state: UsageState::MissingUsage,
                finished_at,
                http_status: status,
                error_kind: error.map(str::to_owned),
                ..UsageOutcome::default()
            },
        )
    }

    pub fn mark_interrupted(
        &self,
        request_id: &str,
        status: Option<u16>,
        error: Option<&str>,
        finished_at: Option<String>,
    ) -> Result<UsageRecord> {
        self.settle_request(
            request_id,
            UsageOutcome {
                state: UsageState::Interrupted,
                finished_at,
                http_status: status,
                error_kind: error.map(str::to_owned),
                ..UsageOutcome::default()
            },
        )
    }

    pub fn get_by_id(&self, request_id: &str) -> Result<Option<UsageRecord>> {
        let conn = self.connection();
        conn.query_row(&format!("SELECT {RECORD_COLUMNS} FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE u.request_id=?1"), params![request_id], row_to_record).optional().map_err(Into::into)
    }

    pub fn list_usage(&self, filter: UsageFilter) -> Result<UsageRecordsPage> {
        let limit = filter.limit.unwrap_or(50).clamp(1, 500);
        let offset = filter.offset.unwrap_or(0);
        let conn = self.connection();
        let mut clauses = vec!["1=1".to_string()];
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(value) = filter.account_id.as_deref() {
            clauses.push("a.upstream_account_id=?".into());
            values.push(Box::new(value.to_owned()));
        }
        if let Some(value) = filter.from.as_deref() {
            clauses.push("u.started_at>=?".into());
            values.push(Box::new(value.to_owned()));
        }
        if let Some(value) = filter.to.as_deref() {
            clauses.push("u.started_at<?".into());
            values.push(Box::new(value.to_owned()));
        }
        if let Some(value) = filter.source.as_deref() {
            clauses.push("u.source=?".into());
            values.push(Box::new(value.to_owned()));
        }
        if let Some(value) = filter.model.as_deref() {
            clauses.push("(u.sent_model=? OR u.requested_model=?)".into());
            values.push(Box::new(value.to_owned()));
            values.push(Box::new(value.to_owned()));
        }
        match filter.downgraded {
            Some(true) => clauses.push("u.downgrade_verdict IS NOT NULL".into()),
            Some(false) => clauses.push("u.downgrade_verdict IS NULL".into()),
            None => {}
        }
        let where_sql = clauses.join(" AND ");
        let count_sql = format!("SELECT COUNT(*) FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE {where_sql}");
        let refs: Vec<&dyn ToSql> = values.iter().map(|v| v.as_ref() as &dyn ToSql).collect();
        let total: u64 = conn
            .query_row(&count_sql, refs.as_slice(), |row| row.get::<_, i64>(0))
            .map(|v| v.max(0) as u64)?;
        let sql = format!("SELECT {RECORD_COLUMNS} FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE {where_sql} ORDER BY u.started_at DESC LIMIT ? OFFSET ?");
        let mut args: Vec<Box<dyn ToSql>> = values;
        args.push(Box::new(limit as i64));
        args.push(Box::new(offset as i64));
        let refs: Vec<&dyn ToSql> = args.iter().map(|v| v.as_ref() as &dyn ToSql).collect();
        let mut stmt = conn.prepare(&sql)?;
        let records = stmt
            .query_map(refs.as_slice(), row_to_record)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(UsageRecordsPage {
            records,
            total,
            limit,
            offset,
        })
    }

    pub fn add_pricing_rule(&self, spec: PricingRuleSpec) -> Result<i64> {
        anyhow::ensure!(
            spec.input_nanos_per_million >= 0
                && spec.cached_input_nanos_per_million >= 0
                && spec.output_nanos_per_million >= 0,
            "pricing must be non-negative"
        );
        let effective = spec
            .effective_from
            .unwrap_or_else(|| "1970-01-01T00:00:00.000Z".into());
        let conn = self.connection();
        conn.execute("INSERT INTO pricing_rules(provider,model,input_nanos_per_million,cached_input_nanos_per_million,output_nanos_per_million,currency,effective_from) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(provider,model,effective_from) DO UPDATE SET input_nanos_per_million=excluded.input_nanos_per_million,cached_input_nanos_per_million=excluded.cached_input_nanos_per_million,output_nanos_per_million=excluded.output_nanos_per_million,currency=excluded.currency", params![spec.provider, spec.model, spec.input_nanos_per_million, spec.cached_input_nanos_per_million, spec.output_nanos_per_million, spec.currency, effective])?;
        self.bump_revision();
        Ok(conn.query_row(
            "SELECT id FROM pricing_rules WHERE provider=?1 AND model=?2 AND effective_from=?3",
            params![spec.provider, spec.model, effective],
            |row| row.get(0),
        )?)
    }

    pub fn account_summaries(&self, filter: UsageFilter) -> Result<BillingSummary> {
        let conn = self.connection();
        let mut args: Vec<Box<dyn ToSql>> = Vec::new();
        let mut clauses = vec!["1=1".to_string()];
        if let Some(value) = filter.from.as_deref() {
            clauses.push("u.started_at>=?".into());
            args.push(Box::new(value.to_owned()));
        }
        if let Some(value) = filter.to.as_deref() {
            clauses.push("u.started_at<?".into());
            args.push(Box::new(value.to_owned()));
        }
        let where_sql = clauses.join(" AND ");
        let refs: Vec<&dyn ToSql> = args.iter().map(|v| v.as_ref() as &dyn ToSql).collect();
        let mut stmt = conn.prepare(&format!("SELECT a.provider,a.upstream_account_id,a.display_email,MIN(u.started_at),MAX(u.started_at),u.source,u.state,COUNT(*),COALESCE(SUM(u.input_tokens),0),COALESCE(SUM(u.cached_input_tokens),0),COALESCE(SUM(u.output_tokens),0),CASE WHEN COUNT(u.cost_nanos)=COUNT(*) THEN SUM(u.cost_nanos) ELSE NULL END,COALESCE(SUM(u.cache_write_tokens),0),COALESCE(SUM(u.reasoning_tokens),0) FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE {where_sql} GROUP BY a.id,u.source,u.state ORDER BY a.provider,a.upstream_account_id"))?;
        let mut grouped: std::collections::BTreeMap<(String, String), AccountSummary> =
            std::collections::BTreeMap::new();
        let rows = stmt.query_map(refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, i64>(12)?,
                row.get::<_, i64>(13)?,
            ))
        })?;
        for row in rows {
            let (
                provider,
                account_id,
                email,
                first,
                last,
                source,
                state,
                count,
                input,
                cached,
                output,
                cost,
                cache_write,
                reasoning,
            ) = row?;
            let item = grouped
                .entry((provider.clone(), account_id.clone()))
                .or_insert_with(|| AccountSummary {
                    provider,
                    account_id,
                    email,
                    first_seen_at: first.clone(),
                    last_seen_at: last.clone(),
                    total: UsageTotals::default(),
                    business: UsageTotals::default(),
                    internal: UsageTotals::default(),
                });
            if first < item.first_seen_at {
                item.first_seen_at = first;
            }
            if last > item.last_seen_at {
                item.last_seen_at = last;
            }
            let destination = if source == "business" {
                &mut item.business
            } else {
                &mut item.internal
            };
            let tokens = [input, cached, cache_write, output, reasoning];
            add_totals(destination, count, &state, tokens, cost);
            add_totals(&mut item.total, count, &state, tokens, cost);
        }
        Ok(BillingSummary {
            generated_at: chrono::Utc::now().to_rfc3339(),
            from: filter.from,
            to: filter.to,
            accounts: grouped.into_values().collect(),
        })
    }
}

/// `tokens` = [input, cached input, cache write, output, reasoning].
fn add_totals(
    target: &mut UsageTotals,
    count: i64,
    state: &str,
    tokens: [i64; 5],
    cost: Option<i64>,
) {
    let [input, cached, cache_write, output, reasoning] = tokens.map(|v| v.max(0) as u64);
    target.request_count = target.request_count.saturating_add(count.max(0) as u64);
    if state == "measured" {
        target.measured_request_count = target
            .measured_request_count
            .saturating_add(count.max(0) as u64);
    } else {
        target.unknown_usage_count = target
            .unknown_usage_count
            .saturating_add(count.max(0) as u64);
    }
    target.input_tokens = target.input_tokens.saturating_add(input);
    target.cached_input_tokens = target.cached_input_tokens.saturating_add(cached);
    target.cache_write_tokens = target.cache_write_tokens.saturating_add(cache_write);
    target.output_tokens = target.output_tokens.saturating_add(output);
    target.reasoning_tokens = target.reasoning_tokens.saturating_add(reasoning);
    if target.cost_complete {
        if let Some(cost) = cost {
            target.cost_nanos = Some(target.cost_nanos.unwrap_or(0).saturating_add(cost));
        } else {
            target.cost_complete = false;
            target.cost_nanos = None;
        }
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<UsageRecord> {
    Ok(UsageRecord {
        request_id: row.get(0)?,
        provider: row.get(1)?,
        account_id: row.get(2)?,
        email: row.get(3)?,
        source: row.get(4)?,
        started_at: row.get(5)?,
        finished_at: row.get(6)?,
        state: UsageState::parse(&row.get::<_, String>(7)?),
        http_status: row
            .get::<_, Option<i64>>(8)?
            .and_then(|v| u16::try_from(v).ok()),
        requested_model: row.get(9)?,
        sent_model: row.get(10)?,
        response_model: row.get(11)?,
        input_tokens: row
            .get::<_, Option<i64>>(12)?
            .and_then(|v| u64::try_from(v).ok()),
        cached_input_tokens: row
            .get::<_, Option<i64>>(13)?
            .and_then(|v| u64::try_from(v).ok()),
        output_tokens: row
            .get::<_, Option<i64>>(14)?
            .and_then(|v| u64::try_from(v).ok()),
        usage_source: row.get(15)?,
        pricing_rule_id: row.get(16)?,
        cost_nanos: row.get(17)?,
        currency: row.get(18)?,
        error_kind: row.get(19)?,
        cache_write_tokens: row
            .get::<_, Option<i64>>(20)?
            .and_then(|v| u64::try_from(v).ok()),
        reasoning_tokens: row
            .get::<_, Option<i64>>(21)?
            .and_then(|v| u64::try_from(v).ok()),
        pricing_model: row.get(22)?,
        service_tier: row.get(23)?,
        long_context: row.get::<_, Option<bool>>(24)?.unwrap_or(false),
        input_cost_nanos: row.get(25)?,
        cache_read_cost_nanos: row.get(26)?,
        cache_write_cost_nanos: row.get(27)?,
        output_cost_nanos: row.get(28)?,
        first_token_ms: row
            .get::<_, Option<i64>>(29)?
            .and_then(|v| u64::try_from(v).ok()),
        transport: row.get(30)?,
        downgrade: row
            .get::<_, Option<String>>(31)?
            .and_then(|json| serde_json::from_str(&json).ok()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(id: &str, account: &str) -> RequestStart {
        RequestStart {
            request_id: id.into(),
            provider: PROVIDER_CHATGPT.into(),
            account_id: account.into(),
            email: Some(format!("{account}@example.com")),
            source: "business".into(),
            started_at: "2026-01-01T00:00:00.000Z".into(),
            requested_model: Some("gpt-test".into()),
            sent_model: Some("gpt-test".into()),
            service_tier: None,
        }
    }

    #[test]
    fn persists_account_separately_and_settlement_is_idempotent() {
        let store = BillingStore::open_in_memory().unwrap();
        store
            .add_pricing_rule(PricingRuleSpec {
                provider: PROVIDER_CHATGPT.into(),
                model: "gpt-test".into(),
                input_nanos_per_million: 1_000_000,
                cached_input_nanos_per_million: 500_000,
                output_nanos_per_million: 2_000_000,
                currency: "USD".into(),
                effective_from: None,
            })
            .unwrap();
        store.begin_request(start("r1", "a")).unwrap();
        let result = store
            .settle_request(
                "r1",
                UsageOutcome {
                    state: UsageState::Measured,
                    finished_at: Some("2026-01-01T00:00:01.000Z".into()),
                    http_status: Some(200),
                    response_model: Some("gpt-test".into()),
                    usage: TokenUsage {
                        input_tokens: Some(1_000),
                        cached_input_tokens: Some(200),
                        output_tokens: Some(500),
                        ..TokenUsage::default()
                    },
                    usage_source: Some("provider_response".into()),
                    ..UsageOutcome::default()
                },
            )
            .unwrap();
        assert_eq!(result.cost_nanos, Some(1_900));
        let again = store
            .settle_request(
                "r1",
                UsageOutcome {
                    state: UsageState::Measured,
                    ..UsageOutcome::default()
                },
            )
            .unwrap();
        assert_eq!(again.cost_nanos, Some(1_900));
        store.begin_request(start("r2", "b")).unwrap();
        let summary = store.account_summaries(UsageFilter::default()).unwrap();
        assert_eq!(summary.accounts.len(), 2);
        assert_eq!(summary.accounts[0].total.request_count, 1);
        assert_eq!(summary.accounts[0].total.cost_nanos, Some(1_900));
    }

    #[test]
    fn pending_rows_are_recovered_as_interrupted() {
        let store = BillingStore::open_in_memory().unwrap();
        store.begin_request(start("r1", "a")).unwrap();
        assert_eq!(store.recover_pending().unwrap(), 1);
        assert_eq!(
            store.get_by_id("r1").unwrap().unwrap().state,
            UsageState::Interrupted
        );
    }

    #[test]
    fn committed_rows_survive_reopening_the_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("billing.sqlite3");
        {
            let store = BillingStore::open(&path, Arc::new(PriceBook::bundled())).unwrap();
            store.begin_request(start("r1", "account-a")).unwrap();
            store
                .mark_missing_usage(
                    "r1",
                    Some(200),
                    None,
                    Some("2026-01-01T00:00:01.000Z".into()),
                )
                .unwrap();
        }
        let reopened = BillingStore::open(&path, Arc::new(PriceBook::bundled())).unwrap();
        let record = reopened.get_by_id("r1").unwrap().unwrap();
        assert_eq!(record.account_id, "account-a");
        assert_eq!(record.state, UsageState::MissingUsage);
        assert_eq!(
            reopened
                .account_summaries(UsageFilter::default())
                .unwrap()
                .accounts[0]
                .total
                .unknown_usage_count,
            1
        );
    }

    #[test]
    fn summary_does_not_report_partial_cost_when_one_row_is_unpriced() {
        let store = BillingStore::open_in_memory().unwrap();
        store
            .add_pricing_rule(PricingRuleSpec {
                provider: PROVIDER_CHATGPT.into(),
                model: "gpt-test".into(),
                input_nanos_per_million: 1_000_000,
                cached_input_nanos_per_million: 500_000,
                output_nanos_per_million: 2_000_000,
                currency: "USD".into(),
                effective_from: None,
            })
            .unwrap();
        store.begin_request(start("priced", "a")).unwrap();
        store
            .settle_request(
                "priced",
                UsageOutcome {
                    state: UsageState::Measured,
                    usage: TokenUsage {
                        input_tokens: Some(1),
                        output_tokens: Some(1),
                        ..TokenUsage::default()
                    },
                    ..UsageOutcome::default()
                },
            )
            .unwrap();
        let mut unpriced = start("unpriced", "a");
        unpriced.sent_model = Some("model-without-a-rule".into());
        unpriced.requested_model = unpriced.sent_model.clone();
        store.begin_request(unpriced).unwrap();
        store
            .settle_request(
                "unpriced",
                UsageOutcome {
                    state: UsageState::Measured,
                    usage: TokenUsage {
                        input_tokens: Some(1),
                        output_tokens: Some(1),
                        ..TokenUsage::default()
                    },
                    ..UsageOutcome::default()
                },
            )
            .unwrap();
        let account = &store
            .account_summaries(UsageFilter::default())
            .unwrap()
            .accounts[0];
        assert_eq!(account.total.request_count, 2);
        assert_eq!(account.total.cost_nanos, None);
        assert_eq!(account.business.cost_nanos, None);
    }

    #[test]
    fn revision_changes_on_every_write() {
        let store = BillingStore::open_in_memory().unwrap();
        let initial = store.revision();
        store.begin_request(start("rev", "a")).unwrap();
        let begun = store.revision();
        assert!(begun > initial);
        store.list_usage(UsageFilter::default()).unwrap();
        assert_eq!(store.revision(), begun, "reads leave it alone");
        store
            .settle_request(
                "rev",
                UsageOutcome {
                    state: UsageState::MissingUsage,
                    ..UsageOutcome::default()
                },
            )
            .unwrap();
        assert!(store.revision() > begun);
    }

    #[test]
    fn downgraded_requests_are_stored_filtered_and_surfaced() {
        let store = BillingStore::open_in_memory().unwrap();
        for (id, served) in [("clean", "gpt-6-astra"), ("rerouted", "gpt-5.6-luna")] {
            let mut request = start(id, "a");
            request.sent_model = Some("gpt-6-astra".into());
            request.requested_model = request.sent_model.clone();
            store.begin_request(request).unwrap();
            let mut signals = DowngradeSignals::default();
            let mut headers = http::HeaderMap::new();
            headers.insert("openai-model", http::HeaderValue::from_static(served));
            signals.observe_headers(&headers);
            store
                .settle_request(
                    id,
                    UsageOutcome {
                        state: UsageState::Measured,
                        downgrade_signals: signals,
                        ..UsageOutcome::default()
                    },
                )
                .unwrap();
        }
        let clean = store.get_by_id("clean").unwrap().unwrap();
        assert!(clean.downgrade.is_none());
        let rerouted = store.get_by_id("rerouted").unwrap().unwrap();
        let report = rerouted.downgrade.unwrap();
        assert_eq!(report.verdict, Verdict::Confirmed);
        assert_eq!(report.effective_model.as_deref(), Some("gpt-5.6-luna"));

        let only = store
            .list_usage(UsageFilter {
                downgraded: Some(true),
                ..UsageFilter::default()
            })
            .unwrap();
        assert_eq!(only.total, 1);
        assert_eq!(only.records[0].request_id, "rerouted");
        let last = store.last_downgrade().unwrap();
        assert_eq!(last.request_id, "rerouted");
    }

    #[test]
    fn catalog_prices_each_bucket_and_tier() {
        let store = BillingStore::open_in_memory().unwrap();
        let mut request = start("codex", "a");
        request.sent_model = Some("gpt-5.1-codex-high".into());
        request.service_tier = Some("priority".into());
        store.begin_request(request).unwrap();
        let record = store
            .settle_request(
                "codex",
                UsageOutcome {
                    state: UsageState::Measured,
                    response_model: Some("gpt-5.1-codex-2026-01-01".into()),
                    service_tier: Some("default".into()),
                    first_token_ms: Some(1_234),
                    transport: Some("http_sse".into()),
                    usage: TokenUsage {
                        input_tokens: Some(10_000),
                        cached_input_tokens: Some(8_000),
                        output_tokens: Some(1_000),
                        reasoning_tokens: Some(400),
                        ..TokenUsage::default()
                    },
                    ..UsageOutcome::default()
                },
            )
            .unwrap();
        assert_eq!(record.pricing_model.as_deref(), Some("gpt-5.1-codex"));
        assert_eq!(record.service_tier.as_deref(), Some("priority"));
        assert_eq!(record.pricing_rule_id, None);
        // Priority: 2000 x $2.5/M + 8000 x $0.25/M + 1000 x $20/M
        assert_eq!(record.input_cost_nanos, Some(5_000_000));
        assert_eq!(record.cache_read_cost_nanos, Some(2_000_000));
        assert_eq!(record.cache_write_cost_nanos, Some(0));
        assert_eq!(record.output_cost_nanos, Some(20_000_000));
        assert_eq!(record.cost_nanos, Some(27_000_000));
        assert_eq!(record.currency.as_deref(), Some("USD"));
        assert_eq!(record.reasoning_tokens, Some(400));
        assert_eq!(record.first_token_ms, Some(1_234));
        assert_eq!(record.transport.as_deref(), Some("http_sse"));
        let totals = &store
            .account_summaries(UsageFilter::default())
            .unwrap()
            .accounts[0]
            .total;
        assert_eq!(totals.reasoning_tokens, 400);
        assert_eq!(totals.cost_nanos, Some(27_000_000));
    }

    #[test]
    fn migrates_version_one_databases() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("billing.sqlite3");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY);\
                 INSERT INTO schema_migrations VALUES (1);\
                 CREATE TABLE accounts (id INTEGER PRIMARY KEY, provider TEXT NOT NULL, upstream_account_id TEXT NOT NULL, display_email TEXT, first_seen_at TEXT NOT NULL, last_seen_at TEXT NOT NULL, UNIQUE(provider, upstream_account_id));\
                 CREATE TABLE usage_records (request_id TEXT PRIMARY KEY, account_id INTEGER NOT NULL, source TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT, state TEXT NOT NULL, http_status INTEGER, requested_model TEXT, sent_model TEXT, response_model TEXT, input_tokens INTEGER, cached_input_tokens INTEGER, output_tokens INTEGER, usage_source TEXT, pricing_rule_id INTEGER, cost_nanos INTEGER, currency TEXT, error_kind TEXT);\
                 INSERT INTO accounts VALUES (1,'chatgpt','old','old@example.com','2026-01-01','2026-01-01');\
                 INSERT INTO usage_records(request_id,account_id,source,started_at,state,input_tokens,output_tokens) VALUES ('old',1,'business','2026-01-01','measured',5,6);",
            )
            .unwrap();
        }
        let store = BillingStore::open(&path, Arc::new(PriceBook::bundled())).unwrap();
        let old = store.get_by_id("old").unwrap().unwrap();
        assert_eq!(old.input_tokens, Some(5));
        assert_eq!(old.cache_write_tokens, None);
        assert!(!old.long_context);
        store.begin_request(start("new", "old")).unwrap();
    }
}
