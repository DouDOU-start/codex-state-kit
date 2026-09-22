import { useEffect, useRef, useState, type ReactNode, type RefObject } from "react";
import Check from "lucide-react/dist/esm/icons/check.js";
import Copy from "lucide-react/dist/esm/icons/copy.js";
import Network from "lucide-react/dist/esm/icons/network.js";
import Route from "lucide-react/dist/esm/icons/route.js";
import Shield from "lucide-react/dist/esm/icons/shield.js";
import TriangleAlert from "lucide-react/dist/esm/icons/triangle-alert.js";
import X from "lucide-react/dist/esm/icons/x.js";
import { isTauri } from "@/lib/api";
import type { LogEntry, Status } from "@/types";

interface NetworkLogDialogProps {
  open: boolean;
  status: Status;
  triggerRef: RefObject<HTMLButtonElement | null>;
  onClose: () => void;
}

function safeNetworkUrl(raw: string, includePath = false): string {
  const value = raw.trim();
  if (!value) return "未配置";
  try {
    const url = new URL(value);
    const port = url.port || (url.protocol === "https:" ? "443" : url.protocol === "http:" ? "80" : "");
    const hostname = url.hostname.replace(/^\[|\]$/g, "");
    const host = hostname.includes(":") ? `[${hostname}]` : hostname;
    const origin = `${url.protocol}//${host}${port ? `:${port}` : ""}`;
    const path = includePath && url.pathname !== "/" ? url.pathname.replace(/\/$/, "") : "";
    return `${origin}${path}`;
  } catch {
    return "已配置（格式无法解析）";
  }
}

function effectiveProxyUrl(raw: string): string {
  const value = raw.trim();
  return value.startsWith("socks5://") ? `socks5h://${value.slice("socks5://".length)}` : value;
}

function ticketRoute(status: Status): string[] {
  if (status.outboundMode === "warp") {
    const endpoint = status.warp.proxyUrl ? safeNetworkUrl(status.warp.proxyUrl) : "WARP 本地端点待连接";
    const exit = status.warp.exitIp
      ? `出口 ${status.warp.exitIp}${status.warp.country ? ` · ${status.warp.country}` : ""}`
      : "出口待验证";
    return ["State Kit", `内置 WARP · ${endpoint} · ${exit}`, safeNetworkUrl(status.upstream, true)];
  }
  return [
    "State Kit",
    status.outboundProxy ? `手动代理 · ${safeNetworkUrl(effectiveProxyUrl(status.outboundProxy))}${status.turnState?.boundProxySession ? ` · session ${status.turnState.boundProxySession}` : ""}` : "手动代理未配置",
    safeNetworkUrl(status.upstream, true),
  ];
}

function businessRoute(status: Status): string[] {
  const sameNetwork = (status.networkRoutePolicy ?? "same_network") === "same_network";
  const hop = sameNetwork
    ? status.outboundMode === "warp"
      ? ticketRoute(status)[1]
      : status.outboundProxy
        ? `同网代理 · ${safeNetworkUrl(effectiveProxyUrl(status.outboundProxy))}${status.turnState?.boundProxySession ? ` · session ${status.turnState.boundProxySession}` : ""}`
        : "同网代理未配置"
    : status.upstreamProxy
      ? `上游转发代理 · ${safeNetworkUrl(effectiveProxyUrl(status.upstreamProxy))}`
      : "系统默认网络（可能受环境代理影响）";
  return [
    "Codex",
    `本机代理 · http://${status.proxyListen}`,
    hop,
    safeNetworkUrl(status.upstream, true),
  ];
}

function routeLabel(entry: LogEntry): string {
  switch (entry.routeKind) {
    case "embedded_warp":
      return `内置 WARP · ${entry.proxyEndpoint || "本地端点"} → ${entry.targetOrigin || "上游"}`;
    case "manual_proxy":
      return `手动代理 · ${entry.proxyEndpoint || "已配置"}${entry.proxySession ? ` · session ${entry.proxySession}` : ""} → ${entry.targetOrigin || "上游"}`;
    case "explicit_proxy":
      return `${entry.proxyEndpoint || "显式代理"} → ${entry.targetOrigin || "上游"}`;
    default:
      return `系统默认网络 → ${entry.targetOrigin || "上游"}`;
  }
}

function stateLabel(entry: LogEntry): string {
  const length = entry.turnStateLen ? ` · ${entry.turnStateLen} 字节` : "";
  switch (entry.turnStateAction) {
    case "replaced": return `已替换${length}`;
    case "replaced_after_wait": return `等待后已替换${length}`;
    case "injected": return `已补上 State${length}`;
    case "injected_after_wait": return `等待后已补上 State${length}`;
    case "header_only": return `同轮续跑，只改请求头${length}`;
    case "header_only_after_wait": return `等待后同轮续跑，只改请求头${length}`;
    case "removed_by_policy": return "按策略剥离 State";
    case "removed_all_policy": return "全部剥离策略，未携带 State";
    case "preserved_by_policy": return `不替换，原样转发${length}`;
    case "state_model_unknown": return "模型未知，未转发";
    case "state_account_unknown": return "账号未知，未转发";
    case "state_account_mismatch": return "账号凭据不匹配，未转发";
    case "state_wait_account_changed": return "账号已变化，等待终止";
    case "state_wait_config_changed": return "线路已变化，等待终止";
    case "state_wait_policy_changed": return "策略已切换，等待终止";
    case "initial_request": return "客户端未携带 State";
    case "preserved_no_ticket": return `沿用客户端值${length}`;
    case "preserved_account_mismatch": return `账号变化，沿用客户端值${length}`;
    case "preserved_unknown_model": return `模型未知，沿用${length}`;
    case "captured": return "已采集入池";
    case "received": return "已收到候选 state";
    case "missing": return "响应未带 state";
    case "rejected_invalid": return "state 无法解析";
    case "rejected_degraded": return "已丢弃降级 state";
    case "rejected_length": return "state 长度不匹配，已拒绝";
    case "rejected_stale": return "state 超过预取年龄，已拒绝";
    case "rejected_future": return "state 时间戳超前，已拒绝";
    case "rejected_status": return "上游状态码异常，已拒绝";
    case "discarded_stale_config": return "配置已变化，已丢弃旧结果";
    case "discarded_stale_account": return "账号已切换，已丢弃旧结果";
    case "removed_account_mismatch": return "账号已切换，已移除旧 state";
    case "pooled_unmatched": return "已入池，但未匹配当前绑定";
    case "awaiting_response": return "未收到响应";
    case "not_applicable":
    case "": return "不适用";
    default: return `未知状态（${entry.turnStateAction}）`;
  }
}

function returnedStateLabel(entry: LogEntry): string {
  return entry.returnedTurnStateLen ? `上游返回 ${entry.returnedTurnStateLen} 字节` : "上游未返回 state";
}

function connectionLabel(entry: LogEntry): string {
  const values = [
    entry.peerAddr ? `TCP peer ${entry.peerAddr}` : "TCP peer 未提供",
    entry.finalOrigin && entry.finalOrigin !== entry.targetOrigin ? `最终 ${entry.finalOrigin}` : null,
  ];
  return values.filter(Boolean).join(" · ");
}

function displayTime(value: string): string {
  const match = value.match(/T(\d{2}:\d{2}:\d{2}\.\d{3})/);
  return match?.[1] || value;
}

function transportLabel(value: string): string {
  return value === "http_sse" ? "HTTP SSE" : "HTTP";
}

function requestLabel(entry: LogEntry): string {
  return entry.flow === "token_fetch" ? "Token 获取" : `${entry.method} ${entry.path}`;
}

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

function formatDuration(ms?: number | null): string {
  if (ms == null) return "—";
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(ms < 10_000 ? 1 : 0)} 秒`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.floor((ms % 60_000) / 1000);
  return `${minutes} 分 ${seconds} 秒`;
}

function speedLabel(entry: LogEntry): string {
  return entry.tokensPerSecond == null ? "— tok/s" : `${entry.tokensPerSecond.toFixed(1)} tok/s`;
}

function accountLabel(status: Status, id?: string | null, email?: string | null): string {
  if (!id) return "账号未知";
  return email || (id === status.currentAccountId ? status.currentAccountEmail : null)
    || status.logs.find((entry) => entry.accountId === id && entry.accountEmail)?.accountEmail
    || "邮箱未知";
}

function filteredLogs(status: Status, account: string): LogEntry[] {
  return account ? status.logs.filter((entry) => entry.accountId === account) : [];
}

function isTokenFetch(entry: LogEntry): boolean {
  return entry.flow === "token_fetch";
}

function policyLabel(entry: LogEntry): string {
  switch (entry.statePolicy) {
    case "preserve": return "无票保留";
    case "wait": return "无票等待";
    case "strip": return "无票剥离";
    case "passthrough": return "不替换";
    case "strip_all": return "全部剥离";
    default: return "未记录";
  }
}

function modelComparison(entry: LogEntry): "一致" | "不一致" | "无法比较" {
  if (!entry.model || !entry.upstreamResponseModel) return "无法比较";
  return entry.model === entry.upstreamResponseModel ? "一致" : "不一致";
}

function ResponseModel({ entry }: { entry: LogEntry }) {
  if (!entry.model && !entry.upstreamResponseModel && entry.transport !== "http_sse") return null;
  const comparison = modelComparison(entry);
  const missing = entry.flow === "token_fetch" ? "未读取（仅响应头）" : entry.inProgress ? "等待返回" : "未获取";
  return (
    <small className={`network-log-model${comparison === "不一致" ? " network-log-model--mismatch" : ""}`} title="按请求模型与上游响应 model 字段的完整名称比较；以结束事件为准。名称差异可能包含版本后缀，不能单凭此字段证明实际运行模型。">
      ↳ 上游响应：{entry.upstreamResponseModel || missing}
      <span className={`network-log-model__badge${comparison === "一致" ? " network-log-model__badge--match" : ""}`}>{comparison === "无法比较" ? comparison : `模型${comparison}`}</span>
    </small>
  );
}

function errorKindLabel(kind: string): string {
  switch (kind) {
    case "stream_idle":
      return "流静默超时";
    case "client_cancelled":
      return "下游已取消";
    case "response_body":
      return "响应体中断";
    case "response_failed":
      return "上游失败";
    case "response_incomplete":
      return "上游未完成";
    default:
      return kind;
  }
}

function streamLabel(entry: LogEntry): string | null {
  const metrics = [
    entry.firstChunkMs != null ? `首块 ${formatDuration(entry.firstChunkMs)}` : null,
    entry.lastChunkMs != null ? `末块 ${formatDuration(entry.lastChunkMs)}` : null,
    entry.streamChunks ? `${entry.streamChunks} 块 · ${formatBytes(entry.streamBytes)}` : null,
    entry.maxIdleMs != null ? `最大静默 ${formatDuration(entry.maxIdleMs)}` : null,
  ].filter(Boolean).join(" · ");
  switch (entry.streamState) {
    case "awaiting_first_chunk":
      return `已收到响应头 · 等待首块${entry.currentIdleMs != null ? ` · 当前静默 ${formatDuration(entry.currentIdleMs)}` : ""}`;
    case "streaming":
      return `流传输中${entry.currentIdleMs != null ? ` · 当前静默 ${formatDuration(entry.currentIdleMs)}` : ""}${metrics ? ` · ${metrics}` : ""}`;
    case "completed":
      return `流已完成${entry.streamTotalMs != null ? ` · 总计 ${formatDuration(entry.streamTotalMs)}` : ""}${metrics ? ` · ${metrics}` : ""}`;
    case "error":
      return `${entry.errorKind === "stream_idle" ? "流静默超时，已断开" : "流读取错误"}${entry.streamTotalMs != null ? ` · ${formatDuration(entry.streamTotalMs)}` : ""}${metrics ? ` · ${metrics}` : ""}`;
    case "cancelled":
      return `下游已取消${entry.streamTotalMs != null ? ` · ${formatDuration(entry.streamTotalMs)}` : ""}${metrics ? ` · ${metrics}` : ""}`;
    default:
      return null;
  }
}

function formatLogEntry(status: Status, entry: LogEntry): string {
  return [
    `[${entry.ts}] ${requestLabel(entry)} -> ${entry.status} (response_header=${entry.responseHeaderMs ?? entry.ms}ms total=${entry.inProgress ? "pending" : `${entry.ms}ms`})`,
    `  account=${accountLabel(status, entry.accountId, entry.accountEmail)}`,
    `  model=${entry.model || "unknown"} transport=${transportLabel(entry.transport)} route=${routeLabel(entry)}`,
    `  upstreamResponseModel=${entry.upstreamResponseModel || "unknown"} modelComparison=${modelComparison(entry)}`,
    `  peer=${entry.peerAddr || "unknown"} final=${entry.finalOrigin || "unknown"} http=${entry.httpVersion || "unknown"} session=${entry.proxySession || "none"}`,
    `  responseHeaderMs=${entry.responseHeaderMs ?? "unknown"} responseEncoding=${entry.responseContentEncoding ?? "unknown"} firstTokenMs=${entry.firstTokenMs ?? "unknown"} totalMs=${entry.inProgress ? "pending" : entry.ms} outputTokens=${entry.outputTokens ?? "unknown"} tokensPerSecond=${entry.tokensPerSecond?.toFixed(1) ?? "unknown"} inProgress=${Boolean(entry.inProgress)}`,
    `  policy=${policyLabel(entry)} state=${stateLabel(entry)} returnedState=${entry.returnedTurnStateLen || 0} body=${formatBytes(entry.bodyBytes)} encoding=${entry.contentEncoding || "none"} error=${entry.errorKind || "none"}`,
    `  stream_state=${entry.streamState || "not_tracked"} first_chunk_ms=${entry.firstChunkMs ?? "none"} last_chunk_ms=${entry.lastChunkMs ?? "none"} total_ms=${entry.streamTotalMs ?? "none"} chunks=${entry.streamChunks || 0} bytes=${entry.streamBytes || 0} max_idle_ms=${entry.maxIdleMs ?? "none"} current_idle_ms=${entry.currentIdleMs ?? "none"}`,
  ].join("\n");
}

function logExport(status: Status, account: string): string {
  const entries = [...filteredLogs(status, account)].reverse();
  const business = entries.filter((entry) => !isTokenFetch(entry));
  const tokenFetch = entries.filter(isTokenFetch);
  return [
    `Token 获取: ${ticketRoute(status).join(" -> ")}`,
    `业务请求: ${businessRoute(status).join(" -> ")}`,
    `账号筛选: ${account ? accountLabel(status, account) : "暂无账号"}`,
    "",
    `## 业务请求 (${business.length})`,
    ...(business.length ? business.map((entry) => formatLogEntry(status, entry)) : ["(无)"]),
    "",
    `## Token 获取 (${tokenFetch.length})`,
    ...(tokenFetch.length ? tokenFetch.map((entry) => formatLogEntry(status, entry)) : ["(无)"]),
  ].join("\n");
}

function LogTable({ status, entries }: { status: Status; entries: LogEntry[] }) {
  return (
    <table className="network-log-table">
      <thead>
        <tr>
          <th scope="col">时间</th>
          <th scope="col">请求</th>
          <th scope="col">结果 / 性能</th>
          <th scope="col">网络路径</th>
          <th scope="col">Turn-State</th>
        </tr>
      </thead>
      <tbody>
        {entries.map((entry) => (
          <tr key={entry.id}>
            <td className="network-log-table__time" title={entry.ts}>{displayTime(entry.ts)}</td>
            <td>
              <strong>{requestLabel(entry)}</strong>
              <small title={accountLabel(status, entry.accountId, entry.accountEmail)}>账号 {accountLabel(status, entry.accountId, entry.accountEmail)}{entry.accountId && entry.accountId === status.currentAccountId ? " · 当前" : ""}</small>
              <small>{isTokenFetch(entry) ? `${entry.method} ${entry.path} · ` : ""}请求模型：{entry.model || "未识别"} · {transportLabel(entry.transport)}</small>
              <ResponseModel entry={entry} />
            </td>
            <td>
              <span className={`network-log-status${entry.status >= 400 || entry.errorKind ? " network-log-status--error" : ""}`}>{entry.status}{entry.inProgress ? " · 进行中" : ""}</span>
              {!isTokenFetch(entry) ? (
                <>
                  <small title="从请求进入代理到首个非空文本、工具参数或图片输出事件；不包含响应头、心跳与预置事件。">首字 {formatDuration(entry.firstTokenMs)} · {speedLabel(entry)}</small>
                  <small title="总耗时统计至响应体结束；tok/s = 上游输出 token 数 ÷（总耗时 − 首字延迟）。">总耗时 {entry.inProgress ? "进行中" : formatDuration(entry.ms)}{entry.outputTokens != null ? ` · ${entry.outputTokens} tokens` : ""}</small>
                </>
              ) : null}
              <small>{entry.httpVersion || "HTTP"} · 响应头 {formatDuration(entry.responseHeaderMs ?? (isTokenFetch(entry) ? entry.ms : null))}{entry.errorKind ? ` · ${errorKindLabel(entry.errorKind)}` : ""}</small>
              {entry.responseContentEncoding && entry.responseContentEncoding !== "none" && <small>响应编码 {entry.responseContentEncoding}</small>}
              {streamLabel(entry) ? <small>{streamLabel(entry)}</small> : null}
            </td>
            <td>
              <span>{routeLabel(entry)}</span>
              <small>{connectionLabel(entry)}</small>
            </td>
            <td>
              <span>{stateLabel(entry)}</span>
              {entry.statePolicy && <small>策略：{policyLabel(entry)}</small>}
              <small>{returnedStateLabel(entry)} · {formatBytes(entry.bodyBytes)} · {entry.contentEncoding || "none"}</small>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  );
}

function LogSection({
  title,
  count,
  empty,
  children,
}: {
  title: string;
  count: number;
  empty: string;
  children: ReactNode;
}) {
  return (
    <section className="network-log-section" aria-label={`${title}，${count} 条`}>
      <header className="network-log-section__head">
        <strong>{title}</strong>
        <span>{count} 条</span>
      </header>
      {count ? children : <p className="network-log-section__empty">{empty}</p>}
    </section>
  );
}

function RouteLine({ icon, label, nodes }: { icon: "ticket" | "business"; label: string; nodes: string[] }) {
  const Icon = icon === "ticket" ? Shield : Network;
  return (
    <div className="network-route-line">
      <span className="network-route-line__label"><Icon size={14} />{label}</span>
      <div className="network-route-line__path">
        {nodes.map((node, index) => (
          <span key={`${node}-${index}`} className="network-route-line__node">
            {index ? <span className="network-route-line__arrow" aria-hidden="true">→</span> : null}
            <span>{node}</span>
          </span>
        ))}
      </div>
    </div>
  );
}

export function NetworkLogDialog({ open, status, triggerRef, onClose }: NetworkLogDialogProps) {
  const dialogRef = useRef<HTMLDialogElement>(null);
  const closeRef = useRef<HTMLButtonElement>(null);
  const [copyState, setCopyState] = useState<"idle" | "done" | "error">("idle");
  const [accountFilter, setAccountFilter] = useState("");

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!open || !dialog) return;
    if (!dialog.open) dialog.showModal();
    closeRef.current?.focus();
    return () => {
      if (dialog.open) dialog.close();
      triggerRef.current?.focus();
    };
  }, [open, triggerRef]);

  useEffect(() => {
    if (copyState === "idle") return;
    const timer = window.setTimeout(() => setCopyState("idle"), 1800);
    return () => window.clearTimeout(timer);
  }, [copyState]);

  if (!open) return null;

  const accounts = [...new Set([status.currentAccountId, ...status.logs.map((entry) => entry.accountId)]
    .filter((id): id is string => Boolean(id)))];
  const selectedAccount = accounts.includes(accountFilter) ? accountFilter : accounts[0] ?? "";

  const copyLogs = async () => {
    try {
      await navigator.clipboard.writeText(logExport(status, selectedAccount));
      setCopyState("done");
    } catch {
      setCopyState("error");
    }
  };

  const entries = [...filteredLogs(status, selectedAccount)].reverse();
  const businessEntries = entries.filter((entry) => !isTokenFetch(entry));
  const tokenEntries = entries.filter(isTokenFetch);
  return (
    <dialog
      ref={dialogRef}
      className="network-log-dialog"
      aria-labelledby="network-log-title"
      aria-modal="true"
      onCancel={(event) => { event.preventDefault(); onClose(); }}
      onClick={(event) => { if (event.target === dialogRef.current) onClose(); }}
    >
      <div className="network-log-dialog__surface">
        <header className="network-log-dialog__header">
          <div className="network-log-dialog__title">
            <span className="network-log-dialog__mark" aria-hidden="true"><Route size={19} /></span>
            <div>
              <h2 id="network-log-title">网络路由日志</h2>
              <p>{isTauri ? `${status.logs.length} 条请求记录 · 自动实时更新` : `${status.logs.length} 条示例记录 · 非实际连接`}</p>
              {status.diagLogPath ? <p className="network-log-diag-path">诊断日志：{status.diagLogPath}</p> : null}
            </div>
          </div>
          <div className="network-log-dialog__actions">
            <button className="button button--secondary network-log-copy" type="button" onClick={() => void copyLogs()} title="复制当前网络日志">
              {copyState === "done" ? <Check size={14} /> : <Copy size={14} />}
              {copyState === "done" ? "已复制" : copyState === "error" ? "复制失败" : "复制日志"}
            </button>
            <button ref={closeRef} className="network-log-close" type="button" aria-label="关闭网络日志" title="关闭" onClick={onClose}>
              <X size={18} />
            </button>
          </div>
        </header>

        <section className="network-route-overview" aria-label="当前网络路径">
          {!isTauri ? (
            <div className="network-log-preview-note" role="note">
              <TriangleAlert size={14} />浏览器预览数据，不代表当前网络连接
            </div>
          ) : null}
          <RouteLine icon="ticket" label="Token 获取" nodes={ticketRoute(status)} />
          <RouteLine icon="business" label="业务请求" nodes={businessRoute(status)} />
        </section>

        <div className="network-log-filter">
          <label>账号
            <select aria-label="筛选日志账号" value={selectedAccount} disabled={!accounts.length} onChange={(event) => { setAccountFilter(event.target.value); setCopyState("idle"); }}>
              {!accounts.length && <option value="">暂无账号</option>}
              {accounts.map((id) => <option key={id} value={id}>{accountLabel(status, id)}</option>)}
            </select>
          </label>
          <span>业务 {businessEntries.length} · Token 获取 {tokenEntries.length} / 共 {status.logs.length} 条 · 按请求发起时的账号记录</span>
        </div>

        <div className="network-log-table-wrap">
          {entries.length ? (
            <>
              <LogSection title="业务请求" count={businessEntries.length} empty="该账号暂无业务请求">
                <LogTable status={status} entries={businessEntries} />
              </LogSection>
              <LogSection title="Token 获取" count={tokenEntries.length} empty="该账号暂无 Token 获取记录">
                <LogTable status={status} entries={tokenEntries} />
              </LogSection>
            </>
          ) : (
            <div className="network-log-dialog__empty">
              <Route size={24} strokeWidth={1.5} />
              <strong>{status.logs.length ? "该账号暂无请求记录" : "等待第一条请求"}</strong>
              <p>业务请求和 Token 获取会分开列在这里。</p>
            </div>
          )}
        </div>
      </div>
    </dialog>
  );
}
