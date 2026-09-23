import { useCallback, useEffect, useMemo, useState } from "react";
import Check from "lucide-react/dist/esm/icons/check.js";
import Copy from "lucide-react/dist/esm/icons/copy.js";
import RefreshCw from "lucide-react/dist/esm/icons/refresh-cw.js";
import Route from "lucide-react/dist/esm/icons/route.js";
import ScrollText from "lucide-react/dist/esm/icons/scroll-text.js";
import { getBillingRecords, isTauri } from "@/lib/api";
import type { BillingRecord, LogEntry, Status } from "@/types";

interface UsageRecordsPanelProps {
  /** 所在 tab 是否可见；切到该 tab 时重新读取记录。 */
  active: boolean;
  status: Status;
}

function formatMoney(costNanos: number | null | undefined): string {
  if (costNanos == null) return "未定价";
  const amount = Number(costNanos) / 1_000_000_000;
  if (!Number.isFinite(amount)) return "未定价";
  const digits = amount < 1 ? 6 : 4;
  return `$${amount.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: digits })}`;
}

function formatTokens(value: number | null | undefined): string {
  if (value == null) return "—";
  return new Intl.NumberFormat("zh-CN").format(value);
}

function formatDuration(ms?: number | null): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return "—";
  if (ms < 1000) return `${Math.round(ms)} ms`;
  const seconds = Math.floor(ms / 1000);
  if (seconds < 60) return `${seconds} 秒`;
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  return `${minutes} 分 ${rest} 秒`;
}

function durationMs(record: BillingRecord): number | null {
  if (!record.finishedAt) return null;
  const ms = Date.parse(record.finishedAt) - Date.parse(record.startedAt);
  return Number.isFinite(ms) && ms >= 0 ? ms : null;
}

function recordClock(record: BillingRecord): { time: string; date: string } {
  const date = new Date(record.startedAt);
  if (Number.isNaN(date.valueOf())) return { time: record.startedAt, date: "" };
  const pad = (value: number) => String(value).padStart(2, "0");
  return {
    time: `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`,
    date: `${date.getFullYear()}/${date.getMonth() + 1}/${date.getDate()}`,
  };
}

function typeLabel(record: BillingRecord): string {
  return record.source === "business" ? "流式" : "请求";
}

function matchLog(record: BillingRecord, logs: LogEntry[]): LogEntry | undefined {
  const start = Date.parse(record.startedAt);
  const model = record.sentModel || record.requestedModel;
  return logs.find((entry) => {
    if (model && entry.model && entry.model !== model) return false;
    const ts = Date.parse(entry.ts);
    return Number.isFinite(ts) && Math.abs(ts - start) < 20_000;
  });
}

function latencyClass(ms: number | null | undefined): string {
  if (ms == null) return "";
  return ms >= 20_000 ? "usage-latency--slow" : "usage-latency--ok";
}

export function UsageRecordsPanel({ active, status }: UsageRecordsPanelProps) {
  const [loading, setLoading] = useState(false);
  const [records, setRecords] = useState<BillingRecord[]>([]);
  const [accountId, setAccountId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [copyState, setCopyState] = useState<"idle" | "done" | "error">("idle");

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const page = await getBillingRecords({ limit: 200, offset: 0 });
      setRecords(page.records);
      setError(null);
      setAccountId((current) => {
        if (current && page.records.some((record) => record.accountId === current)) return current;
        return status.currentAccountId && page.records.some((record) => record.accountId === status.currentAccountId)
          ? status.currentAccountId
          : page.records[0]?.accountId ?? "";
      });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }, [status.currentAccountId]);

  useEffect(() => {
    if (active) void load();
  }, [active, load]);

  useEffect(() => {
    if (copyState === "idle") return;
    const timer = window.setTimeout(() => setCopyState("idle"), 1800);
    return () => window.clearTimeout(timer);
  }, [copyState]);

  const accounts = useMemo(() => {
    const map = new Map<string, string>();
    for (const record of records) map.set(record.accountId, record.email || record.accountId);
    return [...map.entries()];
  }, [records]);

  const visible = records.filter((record) => !accountId || record.accountId === accountId);

  const copyRows = async () => {
    const text = visible.map((record) => {
      const clock = recordClock(record);
      const log = matchLog(record, status.logs);
      return [
        clock.time,
        clock.date,
        record.sentModel || record.requestedModel || "未知模型",
        typeLabel(record),
        `首字 ${formatDuration(log?.firstTokenMs)}`,
        `总耗时 ${formatDuration(log?.ms ?? durationMs(record))}`,
        `in ${record.inputTokens ?? "—"}`,
        `out ${record.outputTokens ?? "—"}`,
        formatMoney(record.costNanos),
      ].join("\t");
    }).join("\n");
    try {
      await navigator.clipboard.writeText(text);
      setCopyState("done");
    } catch {
      setCopyState("error");
    }
  };

  return (
    <div className="usage-records">
      <header className="usage-records__header">
        <div className="section-heading">
          <span className="section-icon"><ScrollText size={19} /></span>
          <div>
            <h2>使用记录</h2>
            <p>{visible.length} 条{!isTauri ? " · 浏览器示例" : ""}</p>
          </div>
        </div>
        <div className="usage-records__actions">
          <button className="billing-panel__refresh" type="button" disabled={loading} onClick={() => void load()}>
            <RefreshCw size={13} className={loading ? "is-spinning" : undefined} />
            {loading ? "读取中" : "刷新"}
          </button>
          <button className="billing-panel__refresh" type="button" disabled={!visible.length} onClick={() => void copyRows()}>
            {copyState === "done" ? <Check size={13} /> : <Copy size={13} />}
            {copyState === "done" ? "已复制" : copyState === "error" ? "复制失败" : "复制记录"}
          </button>
        </div>
      </header>
      <div className="usage-record-filter">
        <label>账号
          <select aria-label="筛选账号" value={accountId} onChange={(event) => setAccountId(event.target.value)}>
            {!accounts.length ? <option value="">暂无账号</option> : null}
            {accounts.map(([id, label]) => <option key={id} value={id}>{label}</option>)}
          </select>
        </label>
        {error ? <span className="usage-record-error">{error}</span> : <span>按请求开始时间排列</span>}
      </div>
      <div className="usage-records__table">
        {visible.length ? (
          <table className="usage-table">
            <thead>
              <tr>
                <th>时间</th>
                <th>模型</th>
                <th>类型</th>
                <th>延迟</th>
                <th>计量</th>
                <th>费用</th>
                <th>客户端</th>
              </tr>
            </thead>
            <tbody>
              {visible.map((record) => {
                const clock = recordClock(record);
                const log = matchLog(record, status.logs);
                const first = log?.firstTokenMs ?? null;
                const total = log?.ms ?? durationMs(record);
                const model = record.sentModel || record.requestedModel || "未知模型";
                return (
                  <tr key={record.requestId}>
                    <td className="usage-table__time">
                      <strong>{clock.time}</strong>
                      <small>{clock.date}</small>
                    </td>
                    <td className="usage-table__model">{model}</td>
                    <td><span className="usage-type">{typeLabel(record)}</span></td>
                    <td className="usage-table__latency">
                      <span className={latencyClass(first)}>首字 {formatDuration(first)}</span>
                      <span className={latencyClass(total)}>总耗时 {formatDuration(total)}</span>
                    </td>
                    <td className="usage-table__meter">
                      <span>↓ {formatTokens(record.inputTokens)}{record.cachedInputTokens ? ` · 缓存 ${formatTokens(record.cachedInputTokens)}` : ""}</span>
                      <span>↑ {formatTokens(record.outputTokens)}</span>
                    </td>
                    <td className="usage-table__cost">{formatMoney(record.costNanos)}</td>
                    <td className="usage-table__client">本机代理</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        ) : (
          <div className="usage-records__empty">
            <Route size={24} strokeWidth={1.5} />
            <strong>还没有使用记录</strong>
            <p>完成一次上游请求后，时间和用量会列在这里。</p>
          </div>
        )}
      </div>
    </div>
  );
}
