import { useCallback, useEffect, useMemo, useState } from "react";
import ReceiptText from "lucide-react/dist/esm/icons/receipt-text.js";
import RefreshCw from "lucide-react/dist/esm/icons/refresh-cw.js";
import TriangleAlert from "lucide-react/dist/esm/icons/triangle-alert.js";
import { getBillingRecords, getBillingSummary, isTauri } from "@/lib/api";
import type {
  BillingAccountSummary,
  BillingRecord,
  BillingSummary,
  BillingUsageTotals,
} from "@/types";

type BillingPeriod = "today" | "month" | "all";

interface BillingPanelProps {
  currentAccountId?: string | null;
  currentAccountEmail?: string | null;
}

function periodBounds(period: BillingPeriod): { from: string | null; to: string | null } {
  if (period === "all") return { from: null, to: null };
  const now = new Date();
  const start = new Date(now);
  if (period === "today") {
    start.setHours(0, 0, 0, 0);
  } else {
    start.setDate(1);
    start.setHours(0, 0, 0, 0);
  }
  return { from: start.toISOString(), to: now.toISOString() };
}

function formatCount(value: number | null | undefined): string {
  return value == null ? "—" : new Intl.NumberFormat("zh-CN").format(value);
}

function formatCost(costNanos: number | null | undefined, currency = "USD"): string {
  if (costNanos == null) return "未定价";
  const amount = Number(costNanos) / 1_000_000_000;
  if (!Number.isFinite(amount)) return "未定价";
  try {
    return new Intl.NumberFormat("zh-CN", { style: "currency", currency, minimumFractionDigits: 4, maximumFractionDigits: 6 }).format(amount);
  } catch {
    return `$${amount.toFixed(4)}`;
  }
}

function accountLabel(account: BillingAccountSummary): string {
  return account.email?.trim() || account.accountId;
}

function sourceLabel(source: string): string {
  if (source === "business") return "业务";
  if (source === "token_fetch") return "Token 获取";
  if (source === "reverify") return "复验";
  return source;
}

function stateLabel(state: string): string {
  if (state === "measured") return "已计价";
  if (state === "missing_usage") return "用量未知";
  if (state === "interrupted") return "中断";
  if (state === "pending") return "待完成";
  return state;
}

function totalOrEmpty(account?: BillingAccountSummary | null): BillingUsageTotals {
  return account?.total ?? {
    requestCount: 0,
    measuredRequestCount: 0,
    unknownUsageCount: 0,
    inputTokens: 0,
    cachedInputTokens: 0,
    outputTokens: 0,
    costNanos: null,
  };
}

function recordTime(record: BillingRecord): string {
  const date = new Date(record.startedAt);
  if (Number.isNaN(date.valueOf())) return record.startedAt;
  return date.toLocaleString("zh-CN", { month: "numeric", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

export function BillingPanel({ currentAccountId, currentAccountEmail }: BillingPanelProps) {
  const [period, setPeriod] = useState<BillingPeriod>("month");
  const [summary, setSummary] = useState<BillingSummary | null>(null);
  const [records, setRecords] = useState<BillingRecord[]>([]);
  const [selectedAccountId, setSelectedAccountId] = useState<string>(currentAccountId ?? "");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const reload = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const bounds = periodBounds(period);
      const [nextSummary, nextRecords] = await Promise.all([
        getBillingSummary(bounds),
        getBillingRecords({ ...bounds, limit: 100, offset: 0 }),
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
  }, [currentAccountId, period]);

  useEffect(() => {
    void reload();
  }, [reload]);

  const accounts = summary?.accounts ?? [];
  const selected = accounts.find((account) => account.accountId === selectedAccountId)
    ?? (currentAccountId ? accounts.find((account) => account.accountId === currentAccountId) : undefined)
    ?? accounts[0];
  const totals = totalOrEmpty(selected);
  const visibleRecords = useMemo(
    () => records.filter((record) => !selected?.accountId || record.accountId === selected.accountId),
    [records, selected?.accountId],
  );

  return (
    <section className="billing-panel panel" aria-label="账号用量与计费">
      <header className="billing-panel__header">
        <div className="section-heading">
          <span className="section-icon section-icon--warm"><ReceiptText size={18} /></span>
          <div>
            <h2>账号用量与计费</h2>
            <p>从本机持久化账单读取，按 ChatGPT 账号归属{!isTauri ? " · 浏览器示例" : ""}</p>
          </div>
        </div>
        <button className="billing-panel__refresh" type="button" onClick={() => void reload()} disabled={loading} title="刷新账单">
          <RefreshCw size={13} className={loading ? "is-spinning" : undefined} />
          {loading ? "读取中" : "刷新"}
        </button>
      </header>

      <div className="billing-panel__toolbar">
        <div className="billing-period" role="tablist" aria-label="账单时间范围">
          <button type="button" role="tab" aria-selected={period === "today"} onClick={() => setPeriod("today")}>今日</button>
          <button type="button" role="tab" aria-selected={period === "month"} onClick={() => setPeriod("month")}>本月</button>
          <button type="button" role="tab" aria-selected={period === "all"} onClick={() => setPeriod("all")}>全部</button>
        </div>
        {accounts.length > 0 ? (
          <label className="billing-account-select">
            <span>账号</span>
            <select value={selected?.accountId ?? ""} onChange={(event) => setSelectedAccountId(event.target.value)}>
              {accounts.map((account) => <option key={account.accountId} value={account.accountId}>{accountLabel(account)}</option>)}
            </select>
          </label>
        ) : null}
      </div>

      {error ? (
        <div className="billing-panel__empty billing-panel__empty--error" role="status">
          <TriangleAlert size={15} />
          <span>{error}</span>
          <button type="button" onClick={() => void reload()}>重试</button>
        </div>
      ) : selected ? (
        <>
          <div className="billing-account-line">
            <strong>{accountLabel(selected)}</strong>
            <span>{selected.accountId}{selected.accountId === currentAccountId ? " · 当前登录" : ""}</span>
            {currentAccountEmail && selected.accountId === currentAccountId && currentAccountEmail !== selected.email ? <span>当前邮箱：{currentAccountEmail}</span> : null}
          </div>
          <dl className="billing-metrics">
            <div><dt>估算成本</dt><dd>{formatCost(totals.costNanos)}</dd></div>
            <div><dt>请求</dt><dd>{formatCount(totals.requestCount)}<small>次</small></dd></div>
            <div><dt>输入 / 缓存</dt><dd>{formatCount(totals.inputTokens)}<small> / {formatCount(totals.cachedInputTokens)}</small></dd></div>
            <div><dt>输出</dt><dd>{formatCount(totals.outputTokens)}<small> tokens</small></dd></div>
            <div title="没有上游用量或尚未完成结算的请求"><dt>用量未知</dt><dd>{formatCount(totals.unknownUsageCount)}<small>次</small></dd></div>
          </dl>
          <div className="billing-subtotals">
            <span>业务 {formatCount(selected.business.requestCount)} 次 · {formatCost(selected.business.costNanos)}</span>
            <span>内部 {formatCount(selected.internal.requestCount)} 次 · {formatCost(selected.internal.costNanos)}</span>
            <span className="billing-panel__note">成本未匹配价格或缺少用量时显示“未定价”</span>
          </div>
          <div className="billing-records">
            <div className="billing-records__heading"><span>最近记录</span><small>{visibleRecords.length} / {records.length}</small></div>
            {visibleRecords.length ? (
              <div className="billing-records__list">
                {visibleRecords.slice(0, 8).map((record) => (
                  <div className="billing-record" key={record.requestId}>
                    <span className="billing-record__time">{recordTime(record)}</span>
                    <span className="billing-record__main"><strong>{record.sentModel || record.requestedModel || "未知模型"}</strong><small>{sourceLabel(record.source)} · {stateLabel(record.state)}</small></span>
                    <span className="billing-record__tokens">{record.inputTokens == null && record.outputTokens == null ? "用量未知" : `${formatCount(record.inputTokens ?? 0)} in · ${formatCount(record.outputTokens ?? 0)} out`}</span>
                    <span className="billing-record__cost">{formatCost(record.costNanos, record.currency || "USD")}</span>
                  </div>
                ))}
              </div>
            ) : <p className="billing-records__empty">该时间范围暂无已持久化记录</p>}
          </div>
        </>
      ) : (
        <div className="billing-panel__empty"><ReceiptText size={16} /><span>还没有持久化用量记录。完成一次上游请求后会在这里显示。</span></div>
      )}
    </section>
  );
}
