import { useEffect, useRef, useState, type RefObject } from "react";
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
    status.outboundProxy ? `手动代理 · ${safeNetworkUrl(effectiveProxyUrl(status.outboundProxy))}` : "手动代理未配置",
    safeNetworkUrl(status.upstream, true),
  ];
}

function businessRoute(status: Status): string[] {
  return [
    "Codex",
    `本机代理 · http://${status.proxyListen}`,
    status.upstreamProxy
      ? `上游转发代理 · ${safeNetworkUrl(effectiveProxyUrl(status.upstreamProxy))}`
      : "系统默认网络（可能受环境代理影响）",
    safeNetworkUrl(status.upstream, true),
  ];
}

function routeLabel(entry: LogEntry): string {
  switch (entry.routeKind) {
    case "embedded_warp":
      return `内置 WARP · ${entry.proxyEndpoint || "本地端点"} → ${entry.targetOrigin || "上游"}`;
    case "manual_proxy":
      return `手动代理 · ${entry.proxyEndpoint || "已配置"} → ${entry.targetOrigin || "上游"}`;
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
    case "initial_request": return "首请求未携带";
    case "preserved_no_ticket": return `沿用客户端值${length}`;
    case "preserved_unknown_model": return `模型未知，沿用${length}`;
    case "captured": return "已采集入池";
    case "received": return "已收到候选 state";
    case "missing": return "响应未带 state";
    case "rejected_invalid": return "state 无法解析";
    case "rejected_degraded": return "已丢弃降级 state";
    default: return "不适用";
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

function logExport(status: Status): string {
  const routeLines = [
    `Token 获取: ${ticketRoute(status).join(" -> ")}`,
    `业务请求: ${businessRoute(status).join(" -> ")}`,
  ];
  const entries = status.logs.map((entry) => [
    `[${entry.ts}] ${requestLabel(entry)} -> ${entry.status} (${entry.ms}ms)`,
    `  model=${entry.model || "unknown"} transport=${transportLabel(entry.transport)} route=${routeLabel(entry)}`,
    `  peer=${entry.peerAddr || "unknown"} final=${entry.finalOrigin || "unknown"} http=${entry.httpVersion || "unknown"}`,
    `  state=${stateLabel(entry)} returnedState=${entry.returnedTurnStateLen || 0} body=${formatBytes(entry.bodyBytes)} encoding=${entry.contentEncoding || "none"} error=${entry.errorKind || "none"}`,
  ].join("\n"));
  return [...routeLines, "", ...entries].join("\n");
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

  const copyLogs = async () => {
    try {
      await navigator.clipboard.writeText(logExport(status));
      setCopyState("done");
    } catch {
      setCopyState("error");
    }
  };

  const entries = [...status.logs].reverse();
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

        <div className="network-log-table-wrap">
          {entries.length ? (
            <table className="network-log-table">
              <thead>
                <tr>
                  <th scope="col">时间</th>
                  <th scope="col">请求</th>
                  <th scope="col">网络路径</th>
                  <th scope="col">Turn-State</th>
                  <th scope="col">结果</th>
                </tr>
              </thead>
              <tbody>
                {entries.map((entry) => (
                  <tr key={entry.id}>
                    <td className="network-log-table__time" title={entry.ts}>{displayTime(entry.ts)}</td>
                    <td>
                      <strong>{requestLabel(entry)}</strong>
                      <small>{entry.flow === "token_fetch" ? `${entry.method} ${entry.path} · ` : ""}{entry.model || "模型未识别"} · {transportLabel(entry.transport)}</small>
                    </td>
                    <td>
                      <span>{routeLabel(entry)}</span>
                      <small>{connectionLabel(entry)}</small>
                    </td>
                    <td>
                      <span>{stateLabel(entry)}</span>
                      <small>{returnedStateLabel(entry)} · {formatBytes(entry.bodyBytes)} · {entry.contentEncoding || "none"}</small>
                    </td>
                    <td>
                      <span className={`network-log-status${entry.status >= 400 ? " network-log-status--error" : ""}`}>{entry.status}</span>
                      <small>{entry.httpVersion || "HTTP"} · 响应头 {entry.ms} ms{entry.errorKind ? ` · ${entry.errorKind}` : ""}</small>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          ) : (
            <div className="network-log-dialog__empty">
              <Route size={24} strokeWidth={1.5} />
              <strong>等待第一条请求</strong>
              <p>Token 获取和 Codex 业务请求会记录在这里。</p>
            </div>
          )}
        </div>
      </div>
    </dialog>
  );
}
