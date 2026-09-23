import { useCallback, useEffect, useRef, useState } from "react";
import BookOpen from "lucide-react/dist/esm/icons/book-open.js";
import Check from "lucide-react/dist/esm/icons/check.js";
import ChevronLeft from "lucide-react/dist/esm/icons/chevron-left.js";
import ChevronRight from "lucide-react/dist/esm/icons/chevron-right.js";
import CircleArrowDown from "lucide-react/dist/esm/icons/circle-arrow-down.js";
import CircleArrowUp from "lucide-react/dist/esm/icons/circle-arrow-up.js";
import PencilLine from "lucide-react/dist/esm/icons/pencil-line.js";
import Copy from "lucide-react/dist/esm/icons/copy.js";
import RefreshCw from "lucide-react/dist/esm/icons/refresh-cw.js";
import Route from "lucide-react/dist/esm/icons/route.js";
import ScrollText from "lucide-react/dist/esm/icons/scroll-text.js";
import { getBillingRecords, getBillingSummary, isTauri } from "@/lib/api";
import { Select } from "@/components/Select";
import { useNotify } from "@/components/Notifier";
import type { BillingRecord, LogEntry, Status } from "@/types";

interface UsageRecordsPanelProps {
  /** 所在 tab 是否可见；切到该 tab 时重新读取记录。 */
  active: boolean;
  status: Status;
}

const PAGE_SIZE = 50;

function formatAmount(costNanos: number): string {
  const amount = Number(costNanos) / 1_000_000_000;
  return amount < 1 ? amount.toFixed(6) : amount.toFixed(4);
}

function formatMoney(costNanos: number | null | undefined): string {
  if (costNanos == null || !Number.isFinite(Number(costNanos))) return "未定价";
  return `$${formatAmount(costNanos)}`;
}

/** 1 万以内显示千分位，更大时显示 K / M。 */
function compactTokens(value: number | null | undefined): string {
  if (value == null) return "—";
  if (value < 10_000) return new Intl.NumberFormat("en-US").format(value);
  if (value < 1_000_000) return `${(value / 1000).toFixed(1)}K`;
  return `${(value / 1_000_000).toFixed(2)}M`;
}

function formatDuration(ms?: number | null): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return "—";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 100_000) return `${(ms / 1000).toFixed(2)}s`;
  const seconds = Math.round(ms / 1000);
  return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
}

type Speed = "fast" | "mid" | "slow" | "none";

/** 首字 5 秒 / 15 秒、总耗时 60 秒 / 180 秒为快慢分界。 */
const FIRST_TOKEN_LIMITS: [number, number] = [5_000, 15_000];
const TOTAL_LIMITS: [number, number] = [60_000, 180_000];

function speed(ms: number | null | undefined, [fast, slow]: [number, number]): Speed {
  if (ms == null || !Number.isFinite(ms)) return "none";
  if (ms < fast) return "fast";
  return ms < slow ? "mid" : "slow";
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

const TRANSPORT_LABEL: Record<string, string> = {
  http: "HTTP",
  http_sse: "HTTP SSE",
  http_to_ws: "HTTP → WebSocket",
  ws_to_ws: "WebSocket",
};

function transportLabel(record: BillingRecord): string {
  return record.transport ? TRANSPORT_LABEL[record.transport] ?? record.transport : "—";
}

/** 上游返回带日期的快照名（如 gpt-5.1-codex-2025-11-13）也算一致。 */
function sameModel(sent: string, response: string): boolean {
  const a = sent.trim().toLowerCase();
  const b = response.trim().toLowerCase();
  return a === b || b.startsWith(`${a}-`);
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


export function UsageRecordsPanel({ active, status }: UsageRecordsPanelProps) {
  const [loading, setLoading] = useState(false);
  const [records, setRecords] = useState<BillingRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(0);
  const [accounts, setAccounts] = useState<[string, string][]>([]);
  /** 空字符串表示全部账号。 */
  const [accountId, setAccountId] = useState("");
  const { notify } = useNotify();
  const reportError = useCallback((cause: unknown) => {
    notify({ kind: "error", title: "读取使用记录失败", message: cause instanceof Error ? cause.message : String(cause) });
  }, [notify]);
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
    } catch (cause) {
      if (seq === requestSeq.current) reportError(cause);
    } finally {
      if (seq === requestSeq.current) setLoading(false);
    }
  }, [reportError]);

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
      reportError(cause);
    }
  }, [status.currentAccountId, reportError]);

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
        transportLabel(record),
        `→ ${record.responseModel ?? "—"}`,
        `首字 ${formatDuration(record.firstTokenMs ?? log?.firstTokenMs)}`,
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
        <div className="usage-record-filter__field">
          <span>账号</span>
          <Select
            variant="compact"
            ariaLabel="筛选账号"
            value={accountId}
            options={[{ value: "", label: "全部账号" }, ...accounts.map(([id, label]) => ({ value: id, label }))]}
            onChange={(next) => {
              accountChosen.current = true;
              setAccountId(next);
              setPage(0);
            }}
          />
        </div>
        <span>按请求开始时间排列</span>
      </div>
      <div className="usage-records__table" ref={tableRef}>
        {visible.length ? (
          <table className="usage-table">
            <thead>
              <tr>
                <th>时间</th>
                <th>模型</th>
                <th>延迟</th>
                <th>计量</th>
                <th>费用</th>
              </tr>
            </thead>
            <tbody>
              {visible.map((record) => {
                const clock = recordClock(record);
                const log = matchLog(record, status.logs);
                const first = record.firstTokenMs ?? log?.firstTokenMs ?? null;
                const total = log?.ms ?? durationMs(record);
                const model = record.sentModel || record.requestedModel || "未知模型";
                const requested = record.requestedModel || model;
                const response = record.responseModel;
                const matches = response ? sameModel(model, response) : null;
                const tier = record.serviceTier ? TIER_LABEL[record.serviceTier] : undefined;
                const parts = costParts(record);
                const cached = record.cachedInputTokens ?? 0;
                const cacheWrite = record.cacheWriteTokens ?? 0;
                const uncached = record.inputTokens == null ? null : Math.max(0, record.inputTokens - cached - cacheWrite);
                const totalTokens = record.inputTokens == null && record.outputTokens == null
                  ? null
                  : (record.inputTokens ?? 0) + (record.outputTokens ?? 0);
                return (
                  <tr key={record.requestId}>
                    <td className="usage-table__time">
                      <strong>{clock.time}</strong>
                      <small>{clock.date}</small>
                    </td>
                    <td className="usage-table__model">
                      <div className="usage-model__line">
                        <span className="usage-model__key">请求模型</span>
                        <strong>{requested}</strong>
                        <span className="usage-model__transport">· {transportLabel(record)}</span>
                      </div>
                      {model !== requested ? (
                        <div className="usage-model__line usage-model__line--sub">
                          <span className="usage-model__key">↳ 转发为</span>
                          <strong>{model}</strong>
                        </div>
                      ) : null}
                      <div className="usage-model__line usage-model__line--sub">
                        <span className="usage-model__key">↳ 上游响应</span>
                        <strong>{response ?? "—"}</strong>
                        {matches === null ? null : (
                          <span className={matches ? "usage-match usage-match--ok" : "usage-match usage-match--bad"}>
                            {matches ? "模型一致" : "模型不一致"}
                          </span>
                        )}
                      </div>
                      {record.pricingModel && record.pricingModel !== model ? <small>按 {record.pricingModel} 计价</small> : null}
                      {tier || record.longContext ? (
                        <span className="usage-table__badges">
                          {tier ? <span className="usage-badge">{tier}</span> : null}
                          {record.longContext ? <span className="usage-badge usage-badge--warm">长上下文</span> : null}
                        </span>
                      ) : null}
                    </td>
                    <td className="usage-table__latency">
                      <div className={`usage-latency usage-latency--${speed(first, FIRST_TOKEN_LIMITS)}`}>
                        <span className="usage-latency__label">首字</span>
                        <span className={`usage-speed--${speed(first, FIRST_TOKEN_LIMITS)}`}>{formatDuration(first)}</span>
                        <span className="usage-latency__label">总耗时</span>
                        <span className={`usage-speed--${speed(total, TOTAL_LIMITS)}`}>{formatDuration(total)}</span>
                      </div>
                    </td>
                    <td className="usage-table__meter">
                      <div className="usage-meter">
                        <div className="usage-meter__io">
                          <span className="usage-meter__in" title="非缓存输入"><CircleArrowDown size={13} />{compactTokens(uncached)}</span>
                          <span className="usage-meter__out" title={record.reasoningTokens ? `输出（含推理 ${compactTokens(record.reasoningTokens)}）` : "输出"}><CircleArrowUp size={13} />{compactTokens(record.outputTokens)}</span>
                          <span className="usage-meter__cache" title="缓存读"><BookOpen size={12} />{compactTokens(record.inputTokens == null ? null : cached)}</span>
                          {cacheWrite ? <span className="usage-meter__cache" title="缓存写"><PencilLine size={12} />{compactTokens(cacheWrite)}</span> : null}
                        </div>
                        <strong className="usage-meter__total" title="总 tokens（输入 + 输出）">{compactTokens(totalTokens)}</strong>
                      </div>
                    </td>
                    <td className="usage-table__cost" title={parts.length ? parts.join("\n") : undefined}>
                      {record.costNanos == null
                        ? <span className="usage-cost usage-cost--none">未定价</span>
                        : <span className="usage-cost"><i>$</i>{formatAmount(record.costNanos)}</span>}
                    </td>
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
