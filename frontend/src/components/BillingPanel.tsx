import { t, formatNumber, getLocale } from "@/lib/i18n";
import { useLocale } from "@/hooks/useLocale";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Select } from "@/components/Select";
import { useNotice } from "@/components/Notifier";
import { RefreshControl } from "@/components/RefreshControl";
import { usePolling } from "@/hooks/usePolling";
import { getBillingRecords, getBillingRevision, getBillingSummary, isTauri } from "@/lib/api";
import { accountLabel } from "@/components/UsageRecordsPanel";
import type { BillingRecord, BillingSummary, BillingUsageTotals, SavedAccount } from "@/types";

interface BillingPanelProps {
  currentAccountId?: string | null;
  currentAccountEmail?: string | null;
  /** Saved logins; label accounts whose records carry no email. */
  savedAccounts: SavedAccount[];
  /** Whether the overview is on screen; auto-refresh only runs then. */
  active: boolean;
  /** Auto-refresh interval; 0 turns it off. */
  refreshMs: number;
  onRefreshMsChange: (intervalMs: number) => void;
}

function daysAgo(days: number): string {
  const date = new Date();
  date.setHours(0, 0, 0, 0);
  date.setDate(date.getDate() - (days - 1));
  return date.toISOString();
}

function dayKey(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.valueOf())) return iso.slice(0, 10);
  return date.toLocaleDateString(getLocale(), { month: "2-digit", day: "2-digit" });
}

function isToday(iso: string): boolean {
  const date = new Date(iso);
  const now = new Date();
  return date.getFullYear() === now.getFullYear()
    && date.getMonth() === now.getMonth()
    && date.getDate() === now.getDate();
}

function formatMoney(costNanos: number | null | undefined): string {
  if (costNanos == null) return "—";
  const amount = Number(costNanos) / 1_000_000_000;
  if (!Number.isFinite(amount)) return "—";
  const digits = amount >= 100 ? 2 : amount >= 1 ? 4 : 6;
  return `$${formatNumber(amount, { minimumFractionDigits: 2, maximumFractionDigits: digits })}`;
}

function formatDay(iso?: string | null): string {
  if (!iso) return "—";
  const date = new Date(iso);
  if (Number.isNaN(date.valueOf())) return iso.slice(0, 10);
  return date.toLocaleDateString(getLocale());
}

/** Show failed and active requests separately from missing usage and prices. */
function unpricedNote(totals?: BillingUsageTotals): string {
  if (!totals) return "";
  const errors = Math.max(0, totals.interruptedRequestCount ?? 0);
  const pending = Math.max(0, totals.pendingRequestCount ?? 0);
  const missing = Math.max(0, totals.missingUsageCount ?? (totals.unknownUsageCount - errors - pending));
  const unpriced = Math.max(0, totals.unpricedCount - totals.unknownUsageCount);
  const parts: string[] = [];
  if (errors > 0) parts.push(t("{0} 条转发错误", [errors]));
  if (pending > 0) parts.push(t("{0} 条处理中", [pending]));
  if (missing > 0) parts.push(t("{0} 条缺少用量", [missing]));
  if (unpriced > 0) parts.push(t("{0} 条价格未匹配", [unpriced]));
  return parts.length ? ` · ${parts.join(" · ")}` : "";
}

function formatTokens(value: number): string {
  if (value >= 1_000_000) return `${formatNumber(value / 1_000_000, { minimumFractionDigits: 2, maximumFractionDigits: 2, useGrouping: false })}M`;
  if (value >= 1_000) return `${formatNumber(value / 1_000, { minimumFractionDigits: 1, maximumFractionDigits: 1, useGrouping: false })}K`;
  return formatNumber(value);
}

function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const digits = ms < 10_000 ? 2 : 1;
  return `${formatNumber(ms / 1000, { minimumFractionDigits: digits, maximumFractionDigits: digits, useGrouping: false })}s`;
}

function sumNanos(records: BillingRecord[]): number | null {
  const known = records.map((record) => record.costNanos).filter((value): value is number => value != null);
  if (!known.length) return null;
  return known.reduce((sum, value) => sum + value, 0);
}

function tokenSum(records: BillingRecord[]): number {
  return records.reduce((sum, record) => sum + (record.inputTokens ?? 0) + (record.outputTokens ?? 0), 0);
}

export function BillingPanel({ currentAccountId, currentAccountEmail, savedAccounts, active, refreshMs, onRefreshMsChange }: BillingPanelProps) {
  useLocale();
  const [summary, setSummary] = useState<BillingSummary | null>(null);
  /** All-time totals, for the cumulative cost. */
  const [lifetime, setLifetime] = useState<BillingSummary | null>(null);
  const [records, setRecords] = useState<BillingRecord[]>([]);
  const [selectedAccountId, setSelectedAccountId] = useState(currentAccountId ?? "");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  /** Revision of the data on screen; auto-refresh reloads when it moves. */
  const shownRevision = useRef<number | null>(null);

  /** `silent`: an auto-refresh, so the refresh button does not spin. */
  const reload = useCallback(async (silent = false) => {
    if (!silent) setLoading(true);
    const from = daysAgo(30);
    const to = new Date().toISOString();
    try {
      // Read the revision first: a write during the query is caught next tick.
      const revision = await getBillingRevision();
      const [nextSummary, nextLifetime, nextRecords] = await Promise.all([
        getBillingSummary({ from, to }),
        getBillingSummary(),
        getBillingRecords({ from, to, limit: 500, offset: 0 }),
      ]);
      setSummary(nextSummary);
      setLifetime(nextLifetime);
      setRecords(nextRecords.records);
      setError(null);
      shownRevision.current = revision;
      // Accounts with only older usage stay selectable.
      const known = nextLifetime.accounts;
      setSelectedAccountId((current) => {
        if (current && known.some((account) => account.accountId === current)) return current;
        if (currentAccountId && known.some((account) => account.accountId === currentAccountId)) return currentAccountId;
        return nextSummary.accounts[0]?.accountId ?? known[0]?.accountId ?? "";
      });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      if (!silent) setLoading(false);
    }
  }, [currentAccountId]);
  useNotice("billing-error", error, () => ({
    kind: "error",
    title: t("读取使用统计失败"),
    message: error,
    actions: [{ label: t("重试"), primary: true, onClick: () => void reload() }],
  }));

  useEffect(() => {
    void reload();
  }, [reload]);

  // Auto-refresh: a cheap revision check each tick, a reload only on change.
  usePolling(async () => {
    if ((await getBillingRevision()) !== shownRevision.current) await reload(true);
  }, refreshMs, active);

  const accounts = lifetime?.accounts ?? summary?.accounts ?? [];
  const chosen = accounts.find((account) => account.accountId === selectedAccountId) ?? accounts[0];
  /** The chosen account's last 30 days; undefined when it has no recent usage. */
  const selected = summary?.accounts.find((account) => account.accountId === chosen?.accountId);
  const allTime = lifetime?.accounts.find((account) => account.accountId === chosen?.accountId);
  const mine = useMemo(
    () => records.filter((record) => !chosen || record.accountId === chosen.accountId),
    [records, chosen],
  );
  const today = mine.filter((record) => isToday(record.startedAt));
  const days = useMemo(() => {
    const buckets = new Map<string, BillingRecord[]>();
    for (const record of mine) {
      const key = dayKey(record.startedAt);
      buckets.set(key, [...(buckets.get(key) ?? []), record]);
    }
    return [...buckets.entries()].map(([label, rows]) => ({
      label,
      requests: rows.length,
      costNanos: sumNanos(rows),
    }));
  }, [mine]);
  const peak = days.reduce<(typeof days)[number] | null>((best, day) => {
    if (day.costNanos == null) return best;
    if (!best || (best.costNanos ?? -1) < day.costNanos) return day;
    return best;
  }, null);
  const durations = mine
    .map((record) => record.finishedAt ? Date.parse(record.finishedAt) - Date.parse(record.startedAt) : NaN)
    .filter((value) => Number.isFinite(value) && value >= 0);
  const averageMs = durations.length ? durations.reduce((sum, value) => sum + value, 0) / durations.length : null;
  const models = useMemo(() => {
    const buckets = new Map<string, { count: number; costNanos: number | null }>();
    for (const record of mine) {
      const name = record.sentModel || record.requestedModel || "";
      const current = buckets.get(name) ?? { count: 0, costNanos: null };
      current.count += 1;
      if (record.costNanos != null) current.costNanos = (current.costNanos ?? 0) + record.costNanos;
      buckets.set(name, current);
    }
    const rows = [...buckets.entries()].map(([name, value]) => ({ name, ...value }));
    const max = Math.max(1, ...rows.map((row) => row.count));
    return rows
      .sort((a, b) => b.count - a.count)
      .map((row) => ({ ...row, width: Math.round((row.count / max) * 100) }));
  }, [mine]);
  const maxRequests = Math.max(1, ...days.map((day) => day.requests));
  const requestCount = selected?.total.requestCount ?? mine.length;
  const activeDays = days.length;
  const dailyRequests = activeDays ? requestCount / activeDays : 0;
  const recentCost = selected?.total.pricedCostNanos ?? 0;
  const dailyCost = activeDays ? recentCost / activeDays : null;

  return (
    <section className="usage-dash panel" aria-label={t("使用统计")}>
      <header className="usage-dash__header">
        <div>
          <h2>{t("使用统计")}</h2>
          <p>{chosen ? t("{0} · 近 30 天用量 · ChatGPT", [accountLabel(chosen.accountId, chosen.email || (chosen.accountId === currentAccountId ? currentAccountEmail : null), savedAccounts)]) : t("近 30 天用量")}{!isTauri ? t(" · 浏览器示例") : ""}</p>
        </div>
        <div className="usage-dash__tools">
          {accounts.length > 1 ? (
            <Select
              variant="compact"
              ariaLabel={t("统计账号")}
              value={chosen?.accountId ?? ""}
              options={accounts.map((account) => ({ value: account.accountId, label: accountLabel(account.accountId, account.email, savedAccounts) }))}
              onChange={setSelectedAccountId}
            />
          ) : null}
          <RefreshControl loading={loading} onRefresh={() => void reload()} intervalMs={refreshMs} onIntervalChange={onRefreshMsChange} />
        </div>
      </header>
      {error ? (
        <div className="billing-panel__empty" role="status">{t("使用统计暂不可用")}</div>
      ) : (
        <>
          <div className="usage-dash__cards">
            <article>
              <span>{t("累计成本")}</span>
              <strong>{formatMoney(allTime ? allTime.total.pricedCostNanos : null)}</strong>
              <small>{allTime ? t("自 {0} · {1} 次请求{2}", [formatDay(allTime.firstSeenAt), formatNumber(allTime.total.requestCount), unpricedNote(allTime.total)]) : t("还没有记录")}</small>
            </article>
            <article><span>{t("30 天总成本")}</span><strong>{formatMoney(recentCost)}</strong><small>{t("已计价请求{0}", [unpricedNote(selected?.total)])}</small></article>
            <article><span>{t("30 天总请求")}</span><strong>{formatNumber(requestCount)}</strong><small>{t("累计调用")}</small></article>
            <article><span>{t("日均成本")}</span><strong>{formatMoney(dailyCost)}</strong><small>{t("基于 {0} 个有数据的日期", [activeDays])}</small></article>
            <article><span>{t("日均请求")}</span><strong>{dailyRequests ? dailyRequests.toFixed(0) : "0"}</strong><small>{t("日均使用量")}</small></article>
          </div>
          <div className="usage-dash__columns">
            <section>
              <h3>{t("今日")}</h3>
              <dl>
                <div><dt>{t("成本")}</dt><dd>{formatMoney(sumNanos(today))}</dd></div>
                <div><dt>{t("请求")}</dt><dd>{today.length}</dd></div>
                <div><dt>Tokens</dt><dd>{formatTokens(tokenSum(today))}</dd></div>
              </dl>
            </section>
            <section>
              <h3>{t("成本最高日")}</h3>
              <dl>
                <div><dt>{t("日期")}</dt><dd>{peak?.label ?? "—"}</dd></div>
                <div><dt>{t("成本")}</dt><dd>{formatMoney(peak?.costNanos)}</dd></div>
                <div><dt>{t("请求")}</dt><dd>{peak?.requests ?? 0}</dd></div>
              </dl>
            </section>
            <section>
              <h3>{t("性能与活跃")}</h3>
              <dl>
                <div><dt>{t("累计 Tokens")}</dt><dd>{formatTokens(tokenSum(mine))}</dd></div>
                <div><dt>{t("平均耗时")}</dt><dd>{averageMs == null ? "—" : formatDuration(averageMs)}</dd></div>
                <div><dt>{t("活跃天数")}</dt><dd>{activeDays} / 30</dd></div>
              </dl>
            </section>
          </div>
          <div className="usage-dash__bottom">
            <section>
              <h3>{t("用量趋势")}</h3>
              {days.length ? (
                <div className="usage-chart" aria-hidden="true">
                  {days.map((day) => (
                    <div key={day.label} className="usage-chart__col" title={t("{0} · {1} 次 · {2}", [day.label, day.requests, formatMoney(day.costNanos)])}>
                      <span style={{ height: `${Math.max(8, Math.round((day.requests / maxRequests) * 72))}px` }} />
                      <small>{day.label.slice(3)}</small>
                    </div>
                  ))}
                </div>
              ) : <p className="usage-dash__empty">{t("还没有可绘制的用量。")}</p>}
            </section>
            <section>
              <h3>{t("模型分布")}</h3>
              {models.length ? models.slice(0, 5).map((model) => (
                <div key={model.name} className="usage-model">
                   <div><strong>{model.name || t("未知模型")}</strong><span>{model.count} · {formatMoney(model.costNanos)}</span></div>
                  <i style={{ width: `${model.width}%` }} />
                </div>
              )) : <p className="usage-dash__empty">{t("完成请求后按模型汇总。")}</p>}
            </section>
          </div>
        </>
      )}
    </section>
  );
}
