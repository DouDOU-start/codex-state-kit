import { useCallback, useEffect, useRef, useState } from "react";
import Check from "lucide-react/dist/esm/icons/check.js";
import ChevronLeft from "lucide-react/dist/esm/icons/chevron-left.js";
import ChevronRight from "lucide-react/dist/esm/icons/chevron-right.js";
import Copy from "lucide-react/dist/esm/icons/copy.js";
import RefreshCw from "lucide-react/dist/esm/icons/refresh-cw.js";
import Route from "lucide-react/dist/esm/icons/route.js";
import ScrollText from "lucide-react/dist/esm/icons/scroll-text.js";
import { getBillingRecords, getBillingSummary, isTauri } from "@/lib/api";
import type { BillingRecord, LogEntry, Status } from "@/types";

interface UsageRecordsPanelProps {
  /** 所在 tab 是否可见；切到该 tab 时重新读取记录。 */
  active: boolean;
  status: Status;
}

const PAGE_SIZE = 50;

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

const TIER_LABEL: Record<string, string> = { priority: "Priority", flex: "Flex" };

/** 非零的分项成本，按 输入 / 缓存读 / 缓存写 / 输出 排列。 */
function costParts(record: BillingRecord): string[] {
  return ([
    ["输入", record.inputCostNanos],
    ["缓存读", record.cacheReadCostNanos],
    ["缓存写", record.cacheWriteCostNanos],
    ["输出", record.outputCostNanos],
  ] as const)
    .filter(([, nanos]) => nanos != null && nanos > 0)
    .map(([label, nanos]) => `${label} ${formatMoney(nanos)}`);
}

function latencyClass(ms: number | null | undefined): string {
  if (ms == null) return "";
  return ms >= 20_000 ? "usage-latency--slow" : "usage-latency--ok";
}

export function UsageRecordsPanel({ active, status }: UsageRecordsPanelProps) {
  const [loading, setLoading] = useState(false);
  const [records, setRecords] = useState<BillingRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(0);
  const [accounts, setAccounts] = useState<[string, string][]>([]);
  /** 空字符串表示全部账号。 */
  const [accountId, setAccountId] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [copyState, setCopyState] = useState<"idle" | "done" | "error">("idle");
  const accountChosen = useRef(false);
  const requestSeq = useRef(0);
  const tableRef = useRef<HTMLDivElement>(null);

  const loadPage = useCallback(async (account: string, pageIndex: number) => {
    const seq = ++requestSeq.current;
    setLoading(true);
    try {
      const result = await getBillingRecords({
        accountId: account || null,
        limit: PAGE_SIZE,
        offset: pageIndex * PAGE_SIZE,
      });
      if (seq !== requestSeq.current) return;
      const lastPage = Math.max(0, Math.ceil(result.total / PAGE_SIZE) - 1);
      if (pageIndex > lastPage) {
        // 记录变少（例如切换账号）后当前页已不存在，回到最后一页。
        setPage(lastPage);
        return;
      }
      setRecords(result.records);
      setTotal(result.total);
      tableRef.current?.scrollTo({ top: 0 });
      setError(null);
    } catch (cause) {
      if (seq === requestSeq.current) setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      if (seq === requestSeq.current) setLoading(false);
    }
  }, []);

  const loadAccounts = useCallback(async () => {
    try {
      const summary = await getBillingSummary();
      const list = summary.accounts.map((account): [string, string] => [account.accountId, account.email || account.accountId]);
      setAccounts(list);
      if (!accountChosen.current) {
        accountChosen.current = true;
        // 默认看当前登录账号；它还没有记录时看全部账号。
        const current = status.currentAccountId;
        if (current && list.some(([id]) => id === current)) setAccountId(current);
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }, [status.currentAccountId]);

  useEffect(() => {
    if (active) void loadAccounts();
  }, [active, loadAccounts]);

  useEffect(() => {
    if (active) void loadPage(accountId, page);
  }, [active, accountId, page, loadPage]);

  useEffect(() => {
    if (copyState === "idle") return;
    const timer = window.setTimeout(() => setCopyState("idle"), 1800);
    return () => window.clearTimeout(timer);
  }, [copyState]);

  const refresh = () => {
    void loadAccounts();
    void loadPage(accountId, page);
  };

  const pageCount = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const visible = records;

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
        `cache_read ${record.cachedInputTokens ?? 0}`,
        `cache_write ${record.cacheWriteTokens ?? 0}`,
        `out ${record.outputTokens ?? "—"}`,
        `reasoning ${record.reasoningTokens ?? 0}`,
        formatMoney(record.costNanos),
        costParts(record).join(" "),
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
            <p>共 {total} 条{!isTauri ? " · 浏览器示例" : ""}</p>
          </div>
        </div>
        <div className="usage-records__actions">
          <button className="billing-panel__refresh" type="button" disabled={loading} onClick={refresh}>
            <RefreshCw size={13} className={loading ? "is-spinning" : undefined} />
            {loading ? "读取中" : "刷新"}
          </button>
          <button className="billing-panel__refresh" type="button" disabled={!visible.length} onClick={() => void copyRows()}>
            {copyState === "done" ? <Check size={13} /> : <Copy size={13} />}
            {copyState === "done" ? "已复制" : copyState === "error" ? "复制失败" : "复制本页"}
          </button>
        </div>
      </header>
      <div className="usage-record-filter">
        <label>账号
          <select
            aria-label="筛选账号"
            value={accountId}
            onChange={(event) => {
              accountChosen.current = true;
              setAccountId(event.target.value);
              setPage(0);
            }}
          >
            <option value="">全部账号</option>
            {accounts.map(([id, label]) => <option key={id} value={id}>{label}</option>)}
          </select>
        </label>
        {error ? <span className="usage-record-error">{error}</span> : <span>按请求开始时间排列</span>}
      </div>
      <div className="usage-records__table" ref={tableRef}>
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
                const tier = record.serviceTier ? TIER_LABEL[record.serviceTier] : undefined;
                const parts = costParts(record);
                return (
                  <tr key={record.requestId}>
                    <td className="usage-table__time">
                      <strong>{clock.time}</strong>
                      <small>{clock.date}</small>
                    </td>
                    <td className="usage-table__model">
                      <strong>{model}</strong>
                      {record.pricingModel && record.pricingModel !== model ? <small>按 {record.pricingModel} 计价</small> : null}
                      {tier || record.longContext ? (
                        <span className="usage-table__badges">
                          {tier ? <span className="usage-badge">{tier}</span> : null}
                          {record.longContext ? <span className="usage-badge usage-badge--warm">长上下文</span> : null}
                        </span>
                      ) : null}
                    </td>
                    <td><span className="usage-type">{typeLabel(record)}</span></td>
                    <td className="usage-table__latency">
                      <span className={latencyClass(first)}>首字 {formatDuration(first)}</span>
                      <span className={latencyClass(total)}>总耗时 {formatDuration(total)}</span>
                    </td>
                    <td className="usage-table__meter">
                      <span>↓ {formatTokens(record.inputTokens)}{record.cachedInputTokens ? ` · 缓存读 ${formatTokens(record.cachedInputTokens)}` : ""}{record.cacheWriteTokens ? ` · 缓存写 ${formatTokens(record.cacheWriteTokens)}` : ""}</span>
                      <span>↑ {formatTokens(record.outputTokens)}{record.reasoningTokens ? ` · 推理 ${formatTokens(record.reasoningTokens)}` : ""}</span>
                    </td>
                    <td className="usage-table__cost">
                      <strong>{formatMoney(record.costNanos)}</strong>
                      {parts.map((part) => <small key={part}>{part}</small>)}
                    </td>
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
      {total > PAGE_SIZE ? (
        <nav className="usage-records__pager" aria-label="使用记录分页">
          <span>第 {page + 1} / {pageCount} 页 · 共 {total} 条</span>
          <div>
            <button className="billing-panel__refresh" type="button" disabled={loading || page === 0} onClick={() => setPage((current) => Math.max(0, current - 1))}>
              <ChevronLeft size={13} />上一页
            </button>
            <button className="billing-panel__refresh" type="button" disabled={loading || page + 1 >= pageCount} onClick={() => setPage((current) => current + 1)}>
              下一页<ChevronRight size={13} />
            </button>
          </div>
        </nav>
      ) : null}
    </div>
  );
}
