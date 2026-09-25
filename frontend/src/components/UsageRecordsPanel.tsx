import { t, formatNumber, getLocale } from "@/lib/i18n";
import { useLocale } from "@/hooks/useLocale";
import { useCallback, useEffect, useRef, useState } from "react";
import BookOpen from "lucide-react/dist/esm/icons/book-open.js";
import CircleArrowDown from "lucide-react/dist/esm/icons/circle-arrow-down.js";
import CircleArrowUp from "lucide-react/dist/esm/icons/circle-arrow-up.js";
import PencilLine from "lucide-react/dist/esm/icons/pencil-line.js";
import Route from "lucide-react/dist/esm/icons/route.js";
import TriangleAlert from "lucide-react/dist/esm/icons/triangle-alert.js";
import ScrollText from "lucide-react/dist/esm/icons/scroll-text.js";
import X from "lucide-react/dist/esm/icons/x.js";
import { getBillingRecords, getBillingRevision, getBillingSummary, isTauri } from "@/lib/api";
import { Select } from "@/components/Select";
import { PAGE_SIZES, Pager } from "@/components/Pager";
import { RefreshControl } from "@/components/RefreshControl";
import { usePolling } from "@/hooks/usePolling";
import { useNotify } from "@/components/Notifier";
import { SHOW_SUSPECTED_DOWNGRADE_UI } from "@/lib/uiFlags";
import type { BillingRecord, DowngradeReport, LogEntry, SavedAccount, Status } from "@/types";

interface UsageRecordsPanelProps {
  /** 所在 tab 是否可见；切到该 tab 时重新读取记录。 */
  active: boolean;
  status: Status;
  /** Saved logins; label accounts whose records carry no email. */
  savedAccounts: SavedAccount[];
  /** Auto-refresh interval; 0 turns it off. */
  refreshMs: number;
  onRefreshMsChange: (intervalMs: number) => void;
}

const PAGE_SIZE_KEY = "codex-state-kit.records-page-size";

function savedPageSize(): number {
  try {
    const saved = Number(window.localStorage.getItem(PAGE_SIZE_KEY));
    if (PAGE_SIZES.includes(saved)) return saved;
  } catch {
    // storage unavailable
  }
  return 50;
}

function formatAmount(costNanos: number): string {
  const amount = Number(costNanos) / 1_000_000_000;
  const digits = amount < 1 ? 6 : 4;
  return formatNumber(amount, { minimumFractionDigits: digits, maximumFractionDigits: digits, useGrouping: false });
}

function formatMoney(costNanos: number | null | undefined): string {
  if (costNanos == null || !Number.isFinite(Number(costNanos))) return t("未定价");
  return `$${formatAmount(costNanos)}`;
}

function unpricedLabel(record: BillingRecord): { label: string; title: string } {
  if (record.state === "pending") {
    return { label: t("处理中"), title: t("请求仍在转发，等待上游返回最终状态。") };
  }
  if (record.state === "interrupted") {
    const reason = record.errorMessage || record.errorKind;
    return { label: t("转发错误"), title: reason ? t("转发失败：{0}", [reason]) : t("请求在转发过程中未完成；历史记录未保存具体错误原因。") };
  }
  if (record.state === "missing_usage" || record.inputTokens == null || record.outputTokens == null) {
    return { label: t("缺少用量"), title: t("请求已返回，但上游没有提供完整的输入/输出 token，暂时无法计算费用。") };
  }
  return { label: t("价格未匹配"), title: t("已有完整用量，但当前价格目录没有匹配的模型价格。") };
}

function stateLabel(state: BillingRecord["state"]): string {
  return ({
    pending: t("处理中"),
    measured: t("已计量"),
    missing_usage: t("缺少用量"),
    interrupted: t("转发错误"),
  } as Record<string, string>)[state] ?? state;
}

/** 1 万以内显示千分位，更大时显示 K / M。 */
function compactTokens(value: number | null | undefined): string {
  if (value == null) return "—";
  if (value < 10_000) return formatNumber(value);
  if (value < 1_000_000) return `${formatNumber(value / 1000, { minimumFractionDigits: 1, maximumFractionDigits: 1, useGrouping: false })}K`;
  return `${formatNumber(value / 1_000_000, { minimumFractionDigits: 2, maximumFractionDigits: 2, useGrouping: false })}M`;
}

function formatDuration(ms?: number | null): string {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return "—";
  if (ms < 1000) return `${Math.round(ms)}ms`;
  if (ms < 100_000) return `${formatNumber(ms / 1000, { minimumFractionDigits: 2, maximumFractionDigits: 2, useGrouping: false })}s`;
  const seconds = Math.round(ms / 1000);
  return `${formatNumber(Math.floor(seconds / 60))}m ${formatNumber(seconds % 60)}s`;
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
    date: date.toLocaleDateString(getLocale()),
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

/** Badge text: confirmed reroutes name the serving model. */
export function downgradeLabel(report: DowngradeReport): string {
  if (report.verdict === "confirmed") {
    return report.effectiveModel ? t("已降级 → {0}", [report.effectiveModel]) : t("已降级");
  }
  if (report.safetyBuffering) return t("疑似降智 · 安全缓冲");
  if (report.verifications?.length) return t("疑似降智 · 需验证");
  return report.effectiveModel ? t("疑似降智 → {0}", [report.effectiveModel]) : t("疑似降智");
}

const TIER_LABEL: Record<string, string> = { priority: "Priority", flex: "Flex" };

/** 非零的分项成本，按 输入 / 缓存读 / 缓存写 / 输出 排列。 */
function costParts(record: BillingRecord): string[] {
  return ([
    [t("输入"), record.inputCostNanos],
    [t("缓存读"), record.cacheReadCostNanos],
    [t("缓存写"), record.cacheWriteCostNanos],
    [t("输出"), record.outputCostNanos],
  ] as const)
    .filter(([, nanos]) => nanos != null && nanos > 0)
    .map(([label, nanos]) => `${label} ${formatMoney(nanos)}`);
}

/** Billing email, else the saved login's email, else the id. */
export function accountLabel(accountId: string, email: string | null | undefined, saved: SavedAccount[]): string {
  if (email) return email;
  const login = saved.find((account) => account.accountId === accountId);
  return login?.email || accountId;
}

export function UsageRecordsPanel({ active, status, savedAccounts, refreshMs, onRefreshMsChange }: UsageRecordsPanelProps) {
  useLocale();
  const [loading, setLoading] = useState(false);
  const [records, setRecords] = useState<BillingRecord[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(0);
  const [pageSize, setPageSize] = useState(savedPageSize);
  const [accounts, setAccounts] = useState<[string, string][]>([]);
  /** 空字符串表示全部账号。 */
  const [accountId, setAccountId] = useState("");
  const [onlyDowngraded, setOnlyDowngraded] = useState(false);
  const [detailRecord, setDetailRecord] = useState<BillingRecord | null>(null);
  const { notify } = useNotify();
  const reportError = useCallback((cause: unknown) => {
    // One notice, updated in place, even when auto-refresh keeps failing.
    notify({ id: "usage-records-error", kind: "error", title: t("读取使用记录失败"), message: cause instanceof Error ? cause.message : String(cause) });
  }, [notify]);
  const accountChosen = useRef(false);
  const requestSeq = useRef(0);
  const tableRef = useRef<HTMLDivElement>(null);
  const detailDialogRef = useRef<HTMLDialogElement>(null);

  /** Revision of the data on screen; auto-refresh reloads when it moves. */
  const shownRevision = useRef<number | null>(null);

  useEffect(() => {
    const dialog = detailDialogRef.current;
    if (!dialog) return;
    if (detailRecord && !dialog.open) dialog.showModal();
    if (!detailRecord && dialog.open) dialog.close();
  }, [detailRecord]);

  /** `silent`: an auto-refresh, so no spinner and no jump back to the top. */
  const loadPage = useCallback(async (account: string, pageIndex: number, downgraded: boolean, size: number, silent = false) => {
    const seq = ++requestSeq.current;
    if (!silent) setLoading(true);
    try {
      // Read the revision first: a write during the query is caught next tick.
      const revision = await getBillingRevision();
      const result = await getBillingRecords({
        accountId: account || null,
        downgraded: downgraded || null,
        limit: size,
        offset: pageIndex * size,
      });
      if (seq !== requestSeq.current) return;
      const lastPage = Math.max(0, Math.ceil(result.total / size) - 1);
      if (pageIndex > lastPage) {
        // 记录变少（例如切换账号）后当前页已不存在，回到最后一页。
        setPage(lastPage);
        return;
      }
      setRecords(result.records);
      setTotal(result.total);
      shownRevision.current = revision;
      if (!silent) tableRef.current?.scrollTo({ top: 0 });
    } catch (cause) {
      if (seq === requestSeq.current) reportError(cause);
    } finally {
      if (seq === requestSeq.current && !silent) setLoading(false);
    }
  }, [reportError]);

  const loadAccounts = useCallback(async () => {
    try {
      const summary = await getBillingSummary();
      const list = summary.accounts.map((account): [string, string] => [account.accountId, account.email || ""]);
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
    if (active) void loadPage(accountId, page, onlyDowngraded, pageSize);
  }, [active, accountId, page, onlyDowngraded, pageSize, loadPage]);

  // Auto-refresh: a cheap revision check each tick, a reload only on change.
  usePolling(async () => {
    const revision = await getBillingRevision();
    if (revision === shownRevision.current) return;
    const knownTotal = total;
    await loadPage(accountId, page, onlyDowngraded, pageSize, true);
    // New records may come from an account not in the filter yet.
    if (knownTotal === 0 || accounts.length === 0) void loadAccounts();
  }, refreshMs, active);

  const changePageSize = (size: number) => {
    setPageSize(size);
    setPage(0);
    try {
      window.localStorage.setItem(PAGE_SIZE_KEY, String(size));
    } catch {
      // storage unavailable
    }
  };

  const explainDowngrade = (record: BillingRecord, report: DowngradeReport) => {
    notify({
      id: `downgrade-${record.requestId}`,
      kind: report.verdict === "confirmed" ? "error" : "warn",
      title: `${downgradeLabel(report)}（${recordClock(record).time}）`,
      message: (
        <ul className="downgrade-signals">
          {report.signals.map((signal) => <li key={signal}>{signal}</li>)}
        </ul>
      ),
    });
  };

  const refresh = () => {
    void loadAccounts();
    void loadPage(accountId, page, onlyDowngraded, pageSize);
  };

  const visible = records;
  const detailLog = detailRecord ? matchLog(detailRecord, status.logs) : undefined;
  const detailError = detailRecord?.errorMessage || detailRecord?.errorKind || detailLog?.errorKind || (detailLog && ["error", "cancelled"].includes(detailLog.streamState) ? `stream_${detailLog.streamState}` : null);
  const detailStatus = detailRecord?.httpStatus ?? detailLog?.status ?? null;
  const detailTransport = detailRecord?.transport || detailLog?.transport || null;

  return (
    <div className="usage-records">
      <header className="usage-records__header">
        <div className="section-heading">
          <span className="section-icon"><ScrollText size={19} /></span>
          <div>
            <h2>{t("使用记录")}</h2>
            <p>{t("共")} {total}  {t("条")}{!isTauri ? t(" · 浏览器示例") : ""}</p>
          </div>
        </div>
        <div className="usage-records__actions">
          <RefreshControl loading={loading} onRefresh={refresh} intervalMs={refreshMs} onIntervalChange={onRefreshMsChange} />
        </div>
      </header>
      <div className="usage-record-filter">
        <div className="usage-record-filter__field">
          <span>{t("账号")}</span>
          <Select
            variant="compact"
            ariaLabel={t("筛选账号")}
            value={accountId}
            options={[{ value: "", label: t("全部账号") }, ...accounts.map(([id, email]) => ({ value: id, label: accountLabel(id, email, savedAccounts) }))]}
            onChange={(next) => {
              accountChosen.current = true;
              setAccountId(next);
              setPage(0);
            }}
          />
        </div>
        <label className="pricing-toggle">
          <input
            type="checkbox"
            checked={onlyDowngraded}
            onChange={(event) => {
              setOnlyDowngraded(event.target.checked);
              setPage(0);
            }}
          />
          {t("只看降智请求")} </label>
        <span>{t("按请求开始时间排列")}</span>
      </div>
      <div className="usage-records__table" ref={tableRef}>
        {visible.length ? (
          <table className="usage-table">
            <thead>
              <tr>
                <th>{t("时间")}</th>
                <th>{t("模型")}</th>
                <th>{t("延迟")}</th>
                <th>{t("计量")}</th>
                <th>{t("费用")}</th>
              </tr>
            </thead>
            <tbody>
              {visible.map((record) => {
                const clock = recordClock(record);
                const log = matchLog(record, status.logs);
                const first = record.firstTokenMs ?? log?.firstTokenMs ?? null;
                const total = log?.ms ?? durationMs(record);
                const model = record.sentModel || record.requestedModel || t("未知模型");
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
                  <tr key={record.requestId} className={record.downgrade && (SHOW_SUSPECTED_DOWNGRADE_UI || record.downgrade.verdict !== "suspected") ? `usage-row--${record.downgrade.verdict}` : undefined}>
                    <td className="usage-table__time">
                      <strong>{clock.time}</strong>
                      <small>{clock.date}</small>
                    </td>
                    <td className="usage-table__model">
                      <div className="usage-model__line">
                        <span className="usage-model__key">{t("请求模型")}</span>
                        <strong>{requested}</strong>
                        <span className="usage-model__transport">· {transportLabel(record)}</span>
                      </div>
                      {model !== requested ? (
                        <div className="usage-model__line usage-model__line--sub">
                          <span className="usage-model__key">{t("↳ 转发为")}</span>
                          <strong>{model}</strong>
                        </div>
                      ) : null}
                      <div className="usage-model__line usage-model__line--sub">
                        <span className="usage-model__key">{t("↳ 上游响应")}</span>
                        <strong>{response ?? "—"}</strong>
                        {matches === null ? null : (
                          <span className={matches ? "usage-match usage-match--ok" : "usage-match usage-match--bad"}>
                            {matches ? t("模型一致") : t("模型不一致")}
                          </span>
                        )}
                      </div>
                      {record.downgrade && (SHOW_SUSPECTED_DOWNGRADE_UI || record.downgrade.verdict !== "suspected") ? (
                        <button
                          type="button"
                          className={`usage-downgrade usage-downgrade--${record.downgrade.verdict}`}
                          title={t("查看判定依据")}
                          onClick={() => explainDowngrade(record, record.downgrade!)}
                        >
                          <TriangleAlert size={11} />
                          {downgradeLabel(record.downgrade)}
                        </button>
                      ) : null}
                      {record.pricingModel && record.pricingModel !== model ? <small>{t("按 {0} 计价", [record.pricingModel])}</small> : null}
                      {tier || record.longContext ? (
                        <span className="usage-table__badges">
                          {tier ? <span className="usage-badge">{tier}</span> : null}
                          {record.longContext ? <span className="usage-badge usage-badge--warm">{t("长上下文")}</span> : null}
                        </span>
                      ) : null}
                    </td>
                    <td className="usage-table__latency">
                      <div className={`usage-latency usage-latency--${speed(first, FIRST_TOKEN_LIMITS)}`}>
                        <span className="usage-latency__label">{t("首字")}</span>
                        <span className={`usage-speed--${speed(first, FIRST_TOKEN_LIMITS)}`}>{formatDuration(first)}</span>
                        <span className="usage-latency__label">{t("总耗时")}</span>
                        <span className={`usage-speed--${speed(total, TOTAL_LIMITS)}`}>{formatDuration(total)}</span>
                      </div>
                    </td>
                    <td className="usage-table__meter">
                      <div className="usage-meter">
                        <div className="usage-meter__io">
                          <span className="usage-meter__in" title={t("非缓存输入")}><CircleArrowDown size={13} />{compactTokens(uncached)}</span>
                          <span className="usage-meter__out" title={record.reasoningTokens ? t("输出（含推理 {0}）", [compactTokens(record.reasoningTokens)]) : t("输出")}><CircleArrowUp size={13} />{compactTokens(record.outputTokens)}</span>
                          <span className="usage-meter__cache" title={t("缓存读")}><BookOpen size={12} />{compactTokens(record.inputTokens == null ? null : cached)}</span>
                          {cacheWrite ? <span className="usage-meter__cache" title={t("缓存写")}><PencilLine size={12} />{compactTokens(cacheWrite)}</span> : null}
                        </div>
                        <strong className="usage-meter__total" title={t("总 tokens（输入 + 输出）")}>{compactTokens(totalTokens)}</strong>
                      </div>
                    </td>
                    <td className="usage-table__cost" title={parts.length ? parts.join("\n") : undefined}>
                      {record.costNanos == null
                        ? (() => {
                          const reason = unpricedLabel(record);
                          return <button type="button" className="usage-cost usage-cost--none usage-cost--button" title={t("{0} 点击查看详情", [reason.title])} onClick={() => setDetailRecord(record)}>{reason.label}</button>;
                        })()
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
            <strong>{onlyDowngraded ? t("没有降智请求") : t("还没有使用记录")}</strong>
            <p>{onlyDowngraded ? t("没有检测到被改路由或安全缓冲的请求。") : t("完成一次上游请求后，时间和用量会列在这里。")}</p>
          </div>
        )}
      </div>
      {total > 0 ? (
        <Pager
          page={page}
          pageSize={pageSize}
          total={total}
          disabled={loading}
          onPage={setPage}
          onPageSize={changePageSize}
        />
      ) : null}
      <dialog
        ref={detailDialogRef}
        className="modal modal--record-detail"
        aria-labelledby="usage-record-detail-title"
        onCancel={(event) => {
          event.preventDefault();
          setDetailRecord(null);
        }}
        onClick={(event) => {
          if (event.target === detailDialogRef.current) setDetailRecord(null);
        }}
      >
        {detailRecord ? (
          <div className="modal__surface">
            <header className="modal__header">
              <div>
                <h2 id="usage-record-detail-title">{t("请求详情")}</h2>
                <p className="modal__subtitle">{recordClock(detailRecord).time} · {recordClock(detailRecord).date}</p>
              </div>
              <button type="button" className="modal__close" aria-label={t("关闭")} onClick={() => setDetailRecord(null)}><X size={16} /></button>
            </header>
            <div className="modal__body record-detail">
              <div className="record-detail__status">
                <strong>{unpricedLabel(detailRecord).label}</strong>
                <span>{stateLabel(detailRecord.state)}</span>
              </div>
              <dl className="record-detail__grid">
                <div><dt>{t("模型")}</dt><dd>{detailRecord.sentModel || detailRecord.requestedModel || "—"}</dd></div>
                <div><dt>{t("上游响应模型")}</dt><dd>{detailRecord.responseModel || "—"}</dd></div>
                <div><dt>{t("HTTP 状态")}</dt><dd>{detailStatus ?? "—"}</dd></div>
                <div><dt>{t("错误类型 / 错误码")}</dt><dd className={detailError ? "record-detail__error" : undefined}>{detailError || "—"}</dd></div>
                <div><dt>{t("传输方式")}</dt><dd>{detailTransport || "—"}</dd></div>
                <div><dt>{t("用量来源")}</dt><dd>{detailRecord.usageSource || "—"}</dd></div>
                <div><dt>{t("输入 tokens")}</dt><dd>{detailRecord.inputTokens ?? "—"}</dd></div>
                <div><dt>{t("输出 tokens")}</dt><dd>{detailRecord.outputTokens ?? "—"}</dd></div>
              </dl>
              <p className="record-detail__hint">{unpricedLabel(detailRecord).title}</p>
              <code className="record-detail__id">{t("请求 ID：")}{detailRecord.requestId}</code>
            </div>
          </div>
        ) : null}
      </dialog>
    </div>
  );
}
