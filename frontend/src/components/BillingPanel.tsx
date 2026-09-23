import { useCallback, useEffect, useMemo, useState } from "react";
import RefreshCw from "lucide-react/dist/esm/icons/refresh-cw.js";
import { Select } from "@/components/Select";
import { useNotice } from "@/components/Notifier";
import { getBillingRecords, getBillingSummary, isTauri } from "@/lib/api";
import type { BillingRecord, BillingSummary } from "@/types";

interface BillingPanelProps {
  currentAccountId?: string | null;
  currentAccountEmail?: string | null;
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
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${month}/${day}`;
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
  return `$${amount.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: digits })}`;
}

function formatTokens(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(2)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return new Intl.NumberFormat("zh-CN").format(value);
}

function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)}s`;
}

function sumNanos(records: BillingRecord[]): number | null {
  const known = records.map((record) => record.costNanos).filter((value): value is number => value != null);
  if (!known.length) return null;
  return known.reduce((sum, value) => sum + value, 0);
}

function tokenSum(records: BillingRecord[]): number {
  return records.reduce((sum, record) => sum + (record.inputTokens ?? 0) + (record.outputTokens ?? 0), 0);
}

export function BillingPanel({ currentAccountId, currentAccountEmail }: BillingPanelProps) {
  const [summary, setSummary] = useState<BillingSummary | null>(null);
  const [records, setRecords] = useState<BillingRecord[]>([]);
  const [selectedAccountId, setSelectedAccountId] = useState(currentAccountId ?? "");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    const from = daysAgo(30);
    const to = new Date().toISOString();
    try {
      const [nextSummary, nextRecords] = await Promise.all([
        getBillingSummary({ from, to }),
        getBillingRecords({ from, to, limit: 500, offset: 0 }),
      ]);
      setSummary(nextSummary);
      setRecords(nextRecords.records);
      setSelectedAccountId((current) => {
        if (current && nextSummary.accounts.some((account) => account.accountId === current)) return current;
        if (currentAccountId && nextSummary.accounts.some((account) => account.accountId === currentAccountId)) return currentAccountId;
        return nextSummary.accounts[0]?.accountId ?? "";
      });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [currentAccountId]);
  useNotice("billing-error", error, () => ({
    kind: "error",
    title: "读取使用统计失败",
    message: error,
    actions: [{ label: "重试", primary: true, onClick: () => void reload() }],
  }));

  useEffect(() => {
    void reload();
  }, [reload]);

  const accounts = summary?.accounts ?? [];
  const selected = accounts.find((account) => account.accountId === selectedAccountId) ?? accounts[0];
  const mine = useMemo(
    () => records.filter((record) => !selected || record.accountId === selected.accountId),
    [records, selected],
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
      const name = record.sentModel || record.requestedModel || "未知模型";
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
  const dailyCost = activeDays && selected?.total.costNanos != null ? selected.total.costNanos / activeDays : null;

  return (
    <section className="usage-dash panel" aria-label="使用统计">
      <header className="usage-dash__header">
        <div>
          <h2>使用统计</h2>
          <p>{selected ? `${selected.email || currentAccountEmail || selected.accountId} · 近 30 天用量 · ChatGPT` : "近 30 天用量"}{!isTauri ? " · 浏览器示例" : ""}</p>
        </div>
        <div className="usage-dash__tools">
          {accounts.length > 1 ? (
            <Select
              variant="compact"
              ariaLabel="统计账号"
              value={selected?.accountId ?? ""}
              options={accounts.map((account) => ({ value: account.accountId, label: account.email || account.accountId }))}
              onChange={setSelectedAccountId}
            />
          ) : null}
          <button type="button" onClick={() => void reload()} disabled={loading}>
            <RefreshCw size={13} className={loading ? "is-spinning" : undefined} />
            {loading ? "读取中" : "刷新"}
          </button>
        </div>
      </header>
      {error ? (
        <div className="billing-panel__empty" role="status">使用统计暂不可用</div>
      ) : (
        <>
          <div className="usage-dash__cards">
            <article><span>30 天总成本</span><strong>{formatMoney(selected?.total.costNanos)}</strong><small>已计价请求</small></article>
            <article><span>30 天总请求</span><strong>{new Intl.NumberFormat("zh-CN").format(requestCount)}</strong><small>累计调用</small></article>
            <article><span>日均成本</span><strong>{formatMoney(dailyCost)}</strong><small>基于 {activeDays} 个有数据的日期</small></article>
            <article><span>日均请求</span><strong>{dailyRequests ? dailyRequests.toFixed(0) : "0"}</strong><small>日均使用量</small></article>
          </div>
          <div className="usage-dash__columns">
            <section>
              <h3>今日</h3>
              <dl>
                <div><dt>成本</dt><dd>{formatMoney(sumNanos(today))}</dd></div>
                <div><dt>请求</dt><dd>{today.length}</dd></div>
                <div><dt>Tokens</dt><dd>{formatTokens(tokenSum(today))}</dd></div>
              </dl>
            </section>
            <section>
              <h3>成本最高日</h3>
              <dl>
                <div><dt>日期</dt><dd>{peak?.label ?? "—"}</dd></div>
                <div><dt>成本</dt><dd>{formatMoney(peak?.costNanos)}</dd></div>
                <div><dt>请求</dt><dd>{peak?.requests ?? 0}</dd></div>
              </dl>
            </section>
            <section>
              <h3>性能与活跃</h3>
              <dl>
                <div><dt>累计 Tokens</dt><dd>{formatTokens(tokenSum(mine))}</dd></div>
                <div><dt>平均耗时</dt><dd>{averageMs == null ? "—" : formatDuration(averageMs)}</dd></div>
                <div><dt>活跃天数</dt><dd>{activeDays} / 30</dd></div>
              </dl>
            </section>
          </div>
          <div className="usage-dash__bottom">
            <section>
              <h3>用量趋势</h3>
              {days.length ? (
                <div className="usage-chart" aria-hidden="true">
                  {days.map((day) => (
                    <div key={day.label} className="usage-chart__col" title={`${day.label} · ${day.requests} 次 · ${formatMoney(day.costNanos)}`}>
                      <span style={{ height: `${Math.max(8, Math.round((day.requests / maxRequests) * 72))}px` }} />
                      <small>{day.label.slice(3)}</small>
                    </div>
                  ))}
                </div>
              ) : <p className="usage-dash__empty">还没有可绘制的用量。</p>}
            </section>
            <section>
              <h3>模型分布</h3>
              {models.length ? models.slice(0, 5).map((model) => (
                <div key={model.name} className="usage-model">
                  <div><strong>{model.name}</strong><span>{model.count} · {formatMoney(model.costNanos)}</span></div>
                  <i style={{ width: `${model.width}%` }} />
                </div>
              )) : <p className="usage-dash__empty">完成请求后按模型汇总。</p>}
            </section>
          </div>
        </>
      )}
    </section>
  );
}
