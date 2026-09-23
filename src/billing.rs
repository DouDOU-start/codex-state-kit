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
use std::sync::{Arc, Mutex};

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
    pub output_tokens: Option<u64>,
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
    pub output_tokens: Option<u64>,
    pub usage_source: Option<String>,
    pub pricing_rule_id: Option<i64>,
    pub cost_nanos: Option<i64>,
    pub currency: Option<String>,
    pub error_kind: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageFilter {
    pub account_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub source: Option<String>,
    pub model: Option<String>,
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
    pub output_tokens: u64,
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
            output_tokens: 0,
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
}

impl BillingStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create billing directory {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("open billing database {}", path.display()))?;
        let store = Self {
            connection: Arc::new(Mutex::new(connection)),
        };
        store.configure()?;
        store.migrate()?;
        store.recover_pending()?;
        Ok(store)
    }

    pub fn open_in_memory() -> Result<Self> {
        let store = Self {
            connection: Arc::new(Mutex::new(Connection::open_in_memory()?)),
        };
        store.configure()?;
        store.migrate()?;
        Ok(store)
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
            "INSERT OR IGNORE INTO usage_records(request_id, account_id, source, started_at, state, requested_model, sent_model) VALUES (?1,?2,?3,?4,'pending',?5,?6)",
            params![start.request_id, account_db_id, start.source, start.started_at, start.requested_model, start.sent_model],
        )?;
        tx.commit()?;
        drop(conn);
        self.get_by_id(&start.request_id)?
            .context("billing insert did not produce a record")
    }

    pub fn settle_request(&self, request_id: &str, outcome: UsageOutcome) -> Result<UsageRecord> {
        let conn = self.connection();
        let tx = conn.unchecked_transaction()?;
        let existing: Option<(String, String, String, Option<String>, String)> = tx
            .query_row("SELECT u.state, u.source, a.provider, u.sent_model, u.started_at FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE u.request_id=?1", params![request_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
            .optional()?;
        let Some((old_state, source, provider, sent_model, started_at)) = existing else {
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
        let (pricing_id, currency, cost) = if measured {
            let pricing_model = outcome.response_model.as_deref().or(sent_model.as_deref());
            self.resolve_cost(
                &tx,
                &provider,
                &source,
                pricing_model,
                &started_at,
                &outcome.usage,
            )?
        } else {
            (None, None, None)
        };
        tx.execute(
            "UPDATE usage_records SET finished_at=?2, state=?3, http_status=?4, response_model=?5, input_tokens=?6, cached_input_tokens=?7, output_tokens=?8, usage_source=?9, pricing_rule_id=?10, cost_nanos=?11, currency=?12, error_kind=?13 WHERE request_id=?1",
            params![request_id, outcome.finished_at, state.as_str(), outcome.http_status.map(i64::from), outcome.response_model, outcome.usage.input_tokens.map(|v| v as i64), outcome.usage.cached_input_tokens.map(|v| v as i64), outcome.usage.output_tokens.map(|v| v as i64), outcome.usage_source, pricing_id, cost, currency, outcome.error_kind],
        )?;
        tx.commit()?;
        drop(conn);
        self.get_by_id(request_id)?
            .context("settled billing record disappeared")
    }

    fn resolve_cost(
        &self,
        tx: &rusqlite::Transaction<'_>,
        provider: &str,
        source: &str,
        model: Option<&str>,
        effective_at: &str,
        usage: &TokenUsage,
    ) -> Result<(Option<i64>, Option<String>, Option<i64>)> {
        if source != "business" {
            return Ok((None, None, None));
        }
        let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else {
            return Ok((None, None, None));
        };
        let rule: Option<(i64, i64, i64, String, i64)> = tx
            .query_row("SELECT input_nanos_per_million, cached_input_nanos_per_million, output_nanos_per_million, currency, id FROM pricing_rules WHERE provider=?1 AND model=?2 AND effective_from <= ?3 ORDER BY effective_from DESC, id DESC LIMIT 1", params![provider, model, effective_at], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
            .optional()?;
        let Some((input_price, cached_price, output_price, currency, id)) = rule else {
            return Ok((None, None, None));
        };
        let input = usage.input_tokens.unwrap_or(0);
        let cached = usage.cached_input_tokens.unwrap_or(0).min(input);
        let output = usage.output_tokens.unwrap_or(0);
        let uncached = input - cached;
        let total = (i128::from(uncached) * i128::from(input_price)
            + i128::from(cached) * i128::from(cached_price)
            + i128::from(output) * i128::from(output_price))
            / 1_000_000;
        let cost = i64::try_from(total).context("billing cost overflow")?;
        Ok((Some(id), Some(currency), Some(cost)))
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
        conn.query_row("SELECT u.request_id,a.provider,a.upstream_account_id,a.display_email,u.source,u.started_at,u.finished_at,u.state,u.http_status,u.requested_model,u.sent_model,u.response_model,u.input_tokens,u.cached_input_tokens,u.output_tokens,u.usage_source,u.pricing_rule_id,u.cost_nanos,u.currency,u.error_kind FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE u.request_id=?1", params![request_id], row_to_record).optional().map_err(Into::into)
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
        let where_sql = clauses.join(" AND ");
        let count_sql = format!("SELECT COUNT(*) FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE {where_sql}");
        let refs: Vec<&dyn ToSql> = values.iter().map(|v| v.as_ref() as &dyn ToSql).collect();
        let total: u64 = conn
            .query_row(&count_sql, refs.as_slice(), |row| row.get::<_, i64>(0))
            .map(|v| v.max(0) as u64)?;
        let sql = format!("SELECT u.request_id,a.provider,a.upstream_account_id,a.display_email,u.source,u.started_at,u.finished_at,u.state,u.http_status,u.requested_model,u.sent_model,u.response_model,u.input_tokens,u.cached_input_tokens,u.output_tokens,u.usage_source,u.pricing_rule_id,u.cost_nanos,u.currency,u.error_kind FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE {where_sql} ORDER BY u.started_at DESC LIMIT ? OFFSET ?");
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
        let mut stmt = conn.prepare(&format!("SELECT a.provider,a.upstream_account_id,a.display_email,MIN(u.started_at),MAX(u.started_at),u.source,u.state,COUNT(*),COALESCE(SUM(u.input_tokens),0),COALESCE(SUM(u.cached_input_tokens),0),COALESCE(SUM(u.output_tokens),0),CASE WHEN COUNT(u.cost_nanos)=COUNT(*) THEN SUM(u.cost_nanos) ELSE NULL END FROM usage_records u JOIN accounts a ON a.id=u.account_id WHERE {where_sql} GROUP BY a.id,u.source,u.state ORDER BY a.provider,a.upstream_account_id"))?;
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
            add_totals(destination, count, &state, input, cached, output, cost);
            add_totals(&mut item.total, count, &state, input, cached, output, cost);
        }
        Ok(BillingSummary {
            generated_at: chrono::Utc::now().to_rfc3339(),
            from: filter.from,
            to: filter.to,
            accounts: grouped.into_values().collect(),
        })
    }
}

fn add_totals(
    target: &mut UsageTotals,
    count: i64,
    state: &str,
    input: i64,
    cached: i64,
    output: i64,
    cost: Option<i64>,
) {
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
    target.input_tokens = target.input_tokens.saturating_add(input.max(0) as u64);
    target.cached_input_tokens = target
        .cached_input_tokens
        .saturating_add(cached.max(0) as u64);
    target.output_tokens = target.output_tokens.saturating_add(output.max(0) as u64);
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
                    },
                    usage_source: Some("provider_response".into()),
                    error_kind: None,
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
            let store = BillingStore::open(&path).unwrap();
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
        let reopened = BillingStore::open(&path).unwrap();
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
}
