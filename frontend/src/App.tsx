import { useEffect, useRef, useState } from "react";
import Copy from "lucide-react/dist/esm/icons/copy.js";
import ExternalLink from "lucide-react/dist/esm/icons/external-link.js";
import LogIn from "lucide-react/dist/esm/icons/log-in.js";
import Shield from "lucide-react/dist/esm/icons/shield.js";
import TriangleAlert from "lucide-react/dist/esm/icons/triangle-alert.js";
import Activity from "lucide-react/dist/esm/icons/activity.js";
import Network from "lucide-react/dist/esm/icons/network.js";
import Terminal from "lucide-react/dist/esm/icons/terminal.js";
import CircleCheck from "lucide-react/dist/esm/icons/circle-check.js";
import Radio from "lucide-react/dist/esm/icons/radio.js";
import Cloud from "lucide-react/dist/esm/icons/cloud.js";
import Play from "lucide-react/dist/esm/icons/play.js";
import Square from "lucide-react/dist/esm/icons/square.js";
import { AppShell } from "@/components/AppShell";
import { NetworkLogDialog } from "@/components/NetworkLogDialog";
import { WarpPanel } from "@/components/WarpPanel";
import { useCodexStateKit } from "@/hooks/useCodexStateKit";
import { isTauri } from "@/lib/api";
import type { LoginMethod, Status, TurnStateView, StateMissPolicy } from "@/types";

function chipLabel(status: Status) {
  if (status.attached) return "已接入";
  return status.proxyOk ? "代理已开" : "代理未开";
}

function chipClass(status: Status) {
  if (status.attached) return "runtime-chip runtime-chip--accent";
  return status.proxyOk ? "runtime-chip" : "runtime-chip runtime-chip--down";
}

function tokenChip(view?: TurnStateView | null) {
  if (!view || view.status === "empty") return { label: "等待 Token", className: "runtime-chip runtime-chip--idle" };
  if (view.status === "idle") return { label: "等待请求", className: "runtime-chip runtime-chip--idle" };
  if (view.status === "active") return { label: "Token 可用", className: "runtime-chip" };
  if (view.status === "partial") return { label: "部分可用", className: "runtime-chip runtime-chip--warm" };
  return { label: "Token 已过期", className: "runtime-chip runtime-chip--warm" };
}

function degradeChip(status: Status) {
  if (!status.degraded) return null;
  return { label: "312 降智", className: "runtime-chip runtime-chip--down" };
}

function formatAge(secs?: number | null) {
  if (secs == null) return null;
  if (secs < 0) return "刚刚";
  if (secs < 60) return `${secs} 秒前`;
  if (secs < 3600) return `${Math.floor(secs / 60)} 分钟前`;
  return `${Math.floor(secs / 3600)} 小时前`;
}

function modelSummary(view?: TurnStateView | null): string {
  const models = view?.models;
  if (!models || models.length === 0) return "";
  const active = models.filter((m) => m.status === "active").length;
  const bound = view?.boundTokenLen ?? 292;
  return `${active}/${models.length} 个模型 Token 就绪 · 绑定 ${bound}`;
}

const STATE_POLICY_OPTIONS: Array<{
  value: StateMissPolicy;
  label: string;
  summary: string;
  tooltip: string;
}> = [
  {
    value: "preserve",
    label: "无票保留",
    summary: "无票时沿用原值",
    tooltip: "优先替换为当前账号、当前模型的有效凭证；没有匹配凭证时保留客户端原 State。默认策略，兼容性优先。",
  },
  {
    value: "wait",
    label: "无票等待",
    summary: "等有效票再发送",
    tooltip: "没有匹配凭证时持续等待，直到取得当前账号、模型和绑定长度的有效 State。请求身份冲突时立即报错，不转发。保证凭证匹配，不代表保证模型智力或回答质量。客户端取消，或账号、线路、策略变化时终止。",
  },
  {
    value: "strip",
    label: "无票剥离",
    summary: "无票时删除",
    tooltip: "优先替换为有效凭证；没有匹配凭证时删除客户端 State，让上游自行处理。用于观察无 State 请求的效果。",
  },
  {
    value: "passthrough",
    label: "不替换",
    summary: "始终沿用原值",
    tooltip: "始终原样转发客户端 State，即使本地有有效凭证也不替换。用于和自动替换策略做对照。",
  },
  {
    value: "strip_all",
    label: "全部剥离",
    summary: "始终删除 State",
    tooltip: "所有业务请求发出前都删除 State，包括本地已有有效凭证的情况。这是实验性策略，效果待验证。",
  },
];

function tokenCopy(
  view?: TurnStateView | null,
  fetchError?: string | null,
  manualModels: string[] = [],
) {
  const bound = view?.boundTokenLen ?? 292;
  if (manualModels.length > 0) {
    return {
      title: `正在手动采集 ${bound} Token…`,
      body: fetchError || manualModels[0],
      loading: true,
    };
  }
  if (fetchError && (!view || (view.status !== "active" && view.status !== "idle"))) {
    return {
      title: `正在获取 ${bound} Token…`,
      body: fetchError,
      loading: true,
    };
  }
  if (!view || view.status === "empty") {
    return {
      title: "等待 Token 就绪",
      body: "登录后自动获取 Token，并在需要时刷新。",
      loading: false,
    };
  }
  if (view.status === "idle") {
    return {
      title: "等待发现模型",
      body: "Codex 发起第一个请求后，自动识别模型并预取 Token。",
      loading: false,
    };
  }
  if (view.status === "active") {
    const summary = modelSummary(view);
    const bound = view.boundTokenLen ?? 292;
    return {
      title: `${bound} Token 正在复用`,
      body: summary,
      loading: false,
    };
  }
  if (view.status === "partial") {
    const summary = modelSummary(view);
    return {
      title: "部分模型 Token 已就绪",
      body: summary || "其余模型正在获取中…",
      loading: true,
    };
  }
  return {
    title: "等待刷新 Token",
    body: "Token 已超过 35 分钟，正在通过出站代理预取新 Token。",
    loading: true,
  };
}

export default function App() {
  const fwd = useCodexStateKit();
  const [codexHome, setCodexHome] = useState("");
  const [outboundProxy, setOutboundProxy] = useState("");
  const [upstreamProxy, setUpstreamProxy] = useState("");
  const [loginMethod, setLoginMethod] = useState<LoginMethod>("browser");
  const [manualModel, setManualModel] = useState("");
  const [networkLogsOpen, setNetworkLogsOpen] = useState(false);
  const networkLogTriggerRef = useRef<HTMLButtonElement>(null);
  const hydrated = useRef(false);

  useEffect(() => {
    if (!fwd.status || hydrated.current) return;
    hydrated.current = true;
    setCodexHome(fwd.status.codexHome);
    setOutboundProxy(fwd.status.outboundProxy ?? "");
    setUpstreamProxy(fwd.status.upstreamProxy ?? "");
  }, [fwd.status]);

  useEffect(() => {
    const models = fwd.status?.availableModels ?? [];
    const active = fwd.status?.manualCollectionModels[0];
    if (!models.length) {
      setManualModel("");
    } else if (active && models.includes(active) && manualModel !== active) {
      setManualModel(active);
    } else if (!models.includes(manualModel)) {
      setManualModel(models[0]);
    }
  }, [fwd.status?.availableModels, fwd.status?.manualCollectionModels, manualModel]);


  if (!fwd.status) {
    return (
      <AppShell>
        <div className="boot-screen">
          {fwd.error ? (
            <div className="boot-screen__error" role="alert">
              <strong>无法启动 Codex State Kit</strong>
              <p>{fwd.error}</p>
              <button className="button button--primary" type="button" onClick={() => void fwd.refresh()}>
                重试
              </button>
            </div>
          ) : (
            <>
              <span className="spinner spinner--blue" />
              正在启动…
            </>
          )}
        </div>
      </AppShell>
    );
  }

  const loggedIn = Boolean(fwd.status.currentAccountId);
  const loginLabel = fwd.status.currentAccountEmail
    || fwd.status.currentAccountId
    || fwd.login?.email
    || fwd.login?.accountId
    || "ChatGPT";
  const manualCollectionModels = fwd.status.manualCollectionModels;
  const turn = fwd.status.turnState;
  const token = tokenChip(turn);
  const degrade = degradeChip(fwd.status);
  const copy = tokenCopy(
    turn,
    fwd.status.fetchError ?? (fwd.status.outboundMode === "warp" ? fwd.status.warp.error : null),
    manualCollectionModels,
  );
  const age = formatAge(turn?.ageSecs);
  const sourceLabel =
    turn?.source === "fetch" ? "StateKit 获取" : turn?.source === "ws" ? "WebSocket" : turn?.source === "http" ? "HTTP" : null;
  const meta = [age, turn?.len ? `${turn.len} 字节` : null, sourceLabel].filter(Boolean).join(" · ");
  const manualCollecting = Boolean(
    manualModel && manualCollectionModels.includes(manualModel),
  );
  const manualReady = Boolean(
    manualModel && turn?.models?.some((model) => model.model === manualModel && model.status === "active"),
  );

  const copyCode = async () => {
    if (!fwd.device?.userCode) return;
    try {
      await navigator.clipboard.writeText(fwd.device.userCode);
    } catch {
      // ignore
    }
  };

  return (
    <AppShell>
      <div className="dash-page">
        <div className="page-heading">
          <div>
            <h1>Codex 稳定助手</h1>
            <p>自动维护 Token，缓解负载与降智问题。</p>
          </div>
          <div className="page-actions">
            <span className={chipClass(fwd.status)}>
              <i />
              {chipLabel(fwd.status)}
            </span>
            <span className={token.className}>
              <i />
              {token.label}
            </span>
            {degrade ? (
              <span className={degrade.className}>
                <i />
                {degrade.label}
              </span>
            ) : null}
          </div>
        </div>

        {fwd.status.proxyError ? (
          <div className="banner banner--error" role="alert">
            <span>{fwd.status.proxyError}</span>
          </div>
        ) : null}

        {fwd.status.attachError ? <div className="banner banner--error" role="alert">{fwd.status.attachError}</div> : null}

        {fwd.banner ? (
          <div className={`banner banner--${fwd.banner.kind}`} role="status">
            <span>{fwd.banner.text}</span>
            <button type="button" onClick={fwd.dismissBanner}>
              关闭
            </button>
          </div>
        ) : null}

        <section className={`token-card token-card--${turn?.status || "empty"}`}>
          <div className="token-card__head">
            <span className="token-card__icon" aria-hidden="true">
              {copy.loading ? <span className="spinner spinner--blue" /> : <Shield size={25} strokeWidth={1.7} />}
            </span>
            <div>
              <span className="token-card__eyebrow">TOKEN 状态</span>
              <strong aria-live="polite">{copy.title}</strong>
              {copy.body ? <small>{copy.body}</small> : null}
              {meta ? <p className="token-card__meta">{meta}</p> : null}
            </div>
          </div>
          <span className="token-card__badge"><Radio size={14} /> {fwd.status.proxyOk ? `自动管理 · 绑定 ${turn?.boundTokenLen ?? 292}` : "等待代理启动"}</span>
          <div className="manual-collection">
            <label htmlFor="manual-collection-model">手动采集</label>
            <select
              id="manual-collection-model"
              value={manualModel}
              disabled={
                fwd.busy !== null
                || !loggedIn
                || fwd.status.availableModels.length === 0
                || fwd.status.manualCollectionModels.length > 0
              }
              onChange={(event) => setManualModel(event.target.value)}
            >
              {fwd.status.availableModels.length > 0
                ? fwd.status.availableModels.map((model) => (
                    <option key={model} value={model}>{model}</option>
                  ))
                : <option value="">未发现模型</option>}
            </select>
            <button
              className={`button ${manualCollecting ? "button--secondary" : "button--primary"}`}
              type="button"
              disabled={
                fwd.busy !== null
                || !loggedIn
                || !manualModel
                || (manualReady && !manualCollecting)
              }
              title={
                manualCollecting
                  ? `停止 ${manualModel} 的手动采集`
                  : manualReady
                    ? `${manualModel} 已有可用 Token`
                    : `开始采集 ${manualModel}`
              }
              onClick={() => {
                if (manualCollecting) {
                  void fwd.stopManualCollection(manualModel);
                } else {
                  void fwd.startManualCollection(manualModel);
                }
              }}
            >
              {manualCollecting
                ? <Square size={13} fill="currentColor" />
                : manualReady
                  ? <CircleCheck size={14} />
                  : <Play size={14} fill="currentColor" />}
              {manualCollecting ? "停止" : manualReady ? "已就绪" : "开始"}
            </button>
          </div>
          {turn?.models && turn.models.length > 0 ? (
            <div className="token-models">
              {turn.models.map((m) => {
                const effectiveBound = m.boundOverride ?? turn.boundTokenLen ?? 292;
                const hasOverride = m.boundOverride != null;
                return (
                <div key={m.model} className="token-model-row">
                  <span className={`token-model token-model--${m.status}`}>
                    <i />{m.model}{m.ageSecs != null ? ` · ${formatAge(m.ageSecs)}` : ""}{m.len ? ` · ${m.len}字节` : ""}
                    {hasOverride ? <span className="token-model__override">独立绑定 {effectiveBound}</span> : null}
                    {manualCollectionModels.includes(m.model) ? <span className="token-model__manual">手动采集中</span> : null}
                  </span>
                  {/* 池中缓存的 token（所有长度），点击设置模型级绑定 */}
                  {m.poolTokens && m.poolTokens.length > 0 ? (
                    <span className="token-pool">
                      {m.poolTokens.map((p) => (
                        <button
                          key={p.len}
                          type="button"
                          className={`token-pool__chip${p.isBound ? " token-pool__chip--bound" : ""}${!p.isValid ? " token-pool__chip--expired" : ""}`}
                          title={
                            p.isBound
                              ? `当前${hasOverride ? "独立" : "全局"}绑定 · ${p.len}字节 · ${formatAge(p.ageSecs)}`
                              : `点击为 ${m.model} 独立绑定 ${p.len}`
                          }
                          onClick={() => {
                            if (p.isBound && hasOverride) {
                              void fwd.bindModelTokenLen(m.model, null);
                            } else {
                              void fwd.bindModelTokenLen(m.model, p.len);
                            }
                          }}
                        >
                          <span className="token-pool__len">{p.len}</span>
                          <span className="token-pool__age">{formatAge(p.ageSecs)}</span>
                          {p.isBound ? <span className="token-pool__bound-tag">{hasOverride ? "独立" : "全局"}</span> : null}
                        </button>
                      ))}
                      {hasOverride ? (
                        <button
                          type="button"
                          className="token-pool__chip token-pool__chip--reset"
                          title="清除模型级绑定，恢复跟随全局"
                          onClick={() => void fwd.bindModelTokenLen(m.model, null)}
                        >
                          ↩ 跟随全局
                        </button>
                      ) : null}
                    </span>
                  ) : null}
                  {/* 最近一轮 fetch 的分布统计 */}
                  {m.distribution && m.distribution.length > 0 ? (
                    <span className="token-dist">
                      <span className="token-dist__label">分布:</span>
                      {m.distribution.map((d) => (
                        <span key={d.len} className="token-dist__item">
                          {d.len}×{d.count}
                        </span>
                      ))}
                    </span>
                  ) : null}
                </div>
                );
              })}
            </div>
          ) : null}
          {fwd.status.fetchError && turn?.status === "active" ? (
            <p className="token-card__meta token-card__meta--warn">刷新失败：{fwd.status.fetchError}</p>
          ) : null}
        </section>

        <section className="account-traffic" aria-label="当前账号请求统计">
          <div className="account-traffic__heading">
            <Activity size={19} aria-hidden="true" />
            <div>
              <h2>当前账号请求</h2>
              <p>{isTauri ? (loggedIn ? "经本机转发的业务请求 · 不含 Token 探测" : "登录后显示账号请求统计") : "浏览器示例数据 · 非实际请求"}</p>
            </div>
          </div>
          <dl className="account-traffic__metrics">
            <div title="此账号已经发起、尚未结束的上游业务请求；包含等待响应和流式输出阶段。">
              <dt>当前并发</dt>
              <dd>{loggedIn ? fwd.status.accountTraffic?.concurrentRequests ?? "—" : "—"}<span>请求</span></dd>
            </div>
            <div title="滚动最近 60 秒内发起的业务请求次数，包括失败请求。">
              <dt>RPM <span>最近 60 秒</span></dt>
              <dd>{loggedIn ? fwd.status.accountTraffic?.rpm ?? "—" : "—"}<span>次 / 分钟</span></dd>
            </div>
          </dl>
        </section>

        {fwd.status.degraded ? (
          <div className="banner banner--error" role="alert">
            <span>
              <TriangleAlert size={14} style={{ verticalAlign: "middle", marginRight: 4 }} />
              检测到 312 降智信号{fwd.status.degradedAt ? `（${fwd.status.degradedAt}）` : ""}，正在通过出站代理重新采集 {turn?.boundTokenLen ?? 292} token…
            </span>
          </div>
        ) : null}

        <div className="panel dash-grid">
        <section className="connection-section panel--proxy">
          <header>
            <div className="section-heading"><span className="section-icon"><Network size={19} /></span><div><h2>Token 获取代理</h2><p>为 Token 获取配置网络</p></div></div>
            <span className="section-step">01</span>
          </header>
          <div className="proxy-mode" role="group" aria-label="出站代理模式">
            <button type="button" aria-pressed={fwd.status.outboundMode === "warp"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveSettings(codexHome, outboundProxy, "warp")}><Cloud size={15} />内置 WARP</button>
            <button type="button" aria-pressed={fwd.status.outboundMode === "manual"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveSettings(codexHome, outboundProxy, "manual")}><Network size={14} />手动代理</button>
          </div>
          {fwd.status.outboundMode === "manual" ? <>
          <label className="field">
            <span>代理 URL</span>
            <input
              type="text"
              spellCheck={false}
              autoComplete="off"
              disabled={fwd.busy !== null}
              value={outboundProxy}
              placeholder="http://127.0.0.1:7890"
              onChange={(event) => setOutboundProxy(event.target.value)}
              onBlur={() => void fwd.saveSettings(codexHome, outboundProxy)}
              onKeyDown={(event) => {
                if (event.key === "Enter") void fwd.saveSettings(codexHome, outboundProxy);
              }}
            />
          </label>
          <p className="panel__hint">支持 socks5 / socks5h / http，离开输入框后自动保存。</p>
          </> : <WarpPanel status={fwd.status.warp} onTerms={() => void fwd.openWarpTerms()} />}
          <label className="field">
            <span>上游转发代理</span>
            <input
              type="text"
              spellCheck={false}
              autoComplete="off"
              disabled={fwd.busy !== null}
              value={upstreamProxy}
              placeholder="http://127.0.0.1:7897"
              onChange={(event) => setUpstreamProxy(event.target.value)}
              onBlur={() => void fwd.saveSettings(codexHome, outboundProxy, undefined, undefined, upstreamProxy)}
              onKeyDown={(event) => {
                if (event.key === "Enter") void fwd.saveSettings(codexHome, outboundProxy, undefined, undefined, upstreamProxy);
              }}
            />
          </label>
          <p className="panel__hint">仅用于业务转发，留空保持默认网络行为。支持 HTTP / HTTPS / SOCKS，失焦或 Enter 自动保存。</p>
        </section>

        <section className="connection-section">
          <header>
            <div className="section-heading"><span className="section-icon section-icon--warm"><Terminal size={19} /></span><div><h2>Codex 接入</h2><p>登录账号，连接你的客户端</p></div></div>
            <span className="section-step">02</span>
          </header>
          <div className="proxy-mode" role="group" aria-label="登录方式">
            <button type="button" aria-pressed={loginMethod === "browser"} disabled={fwd.busy !== null || Boolean(fwd.device)} onClick={() => setLoginMethod("browser")}><ExternalLink size={14} />浏览器回调</button>
            <button type="button" aria-pressed={loginMethod === "device"} disabled={fwd.busy !== null || Boolean(fwd.device)} onClick={() => setLoginMethod("device")}><Copy size={14} />授权码登录</button>
          </div>
          <div className="login-box">
            <span className="field-label">ChatGPT 账号 {loggedIn && !fwd.device ? <span className="account-status"><CircleCheck size={12} /> 已登录</span> : null}</span>
            {fwd.device ? (
              <div className="login-pending">
                <p>{fwd.device.method === "browser" ? "请在浏览器完成授权，登录结果将自动同步。" : "在浏览器打开验证页并输入代码"}</p>
                {fwd.device.method === "device" ? <div className="user-code">{fwd.device.userCode}</div> : null}
                <div className="panel__actions">
                  {fwd.device.method === "device" ? <button className="button button--secondary" type="button" onClick={() => void copyCode()}>
                    <Copy size={14} />
                    复制
                  </button> : null}
                  <button className="button button--secondary" type="button" onClick={() => void fwd.openLoginPage()}>
                    <ExternalLink size={14} />
                    打开页面
                  </button>
                  <button className="button button--ghost" type="button" onClick={() => void fwd.cancelLogin()}>
                    取消
                  </button>
                </div>
              </div>
            ) : loggedIn ? (
              <div className="login-current">
                <div>
                  <strong>{loginLabel}</strong>
                  {fwd.login?.accountId && fwd.login.email ? (
                    <span className="login-meta">{fwd.login.accountId}</span>
                  ) : null}
                </div>
                <button
                  className="button button--ghost"
                  type="button"
                  disabled={fwd.busy !== null}
                  onClick={() => void fwd.startLogin(codexHome, loginMethod)}
                >
                  {fwd.busy === "login" ? <span className="spinner" /> : <LogIn size={14} />}
                  重新登录
                </button>
              </div>
            ) : (
              <div className="login-current">
                <span>尚未登录 ChatGPT</span>
                <button
                  className="button button--primary"
                  type="button"
                  disabled={fwd.busy !== null}
                  onClick={() => void fwd.startLogin(codexHome, loginMethod)}
                >
                  {fwd.busy === "login" ? <span className="spinner" /> : <LogIn size={14} />}
                  登录 ChatGPT
                </button>
              </div>
            )}
          </div>
          <p className="panel__hint">
            启动后自动接入。运行期间把 Kit 账号同步到 Codex 窗口和用量，并按官方登录生成模型目录（含思考等级 ultra 和 Fast）；关闭时还原原来的官方账号、路由和本地模型配置。若窗口还没刷新，重开一次 Codex 即可。
          </p>
        </section>
          <label className="field connection-directory">
            <span>Codex 工作目录</span>
            <input spellCheck={false} disabled={fwd.busy !== null} value={codexHome} onChange={(event) => setCodexHome(event.target.value)}
              onBlur={() => void fwd.saveSettings(codexHome, outboundProxy)}
              onKeyDown={(event) => { if (event.key === "Enter") event.currentTarget.blur(); }} />
          </label>
          <div className="connection-policy">
            <div className="field connection-directory state-policy-field">
              <span>State 处理策略</span>
              <div className="state-policy-options" role="radiogroup" aria-label="State 处理策略">
                {STATE_POLICY_OPTIONS.map((option) => (
                  <label
                    key={option.value}
                    className={`state-policy-option${(fwd.status?.stateMissPolicy ?? "preserve") === option.value ? " state-policy-option--selected" : ""}`}
                    data-tooltip={option.tooltip}
                  >
                    <input
                      type="radio"
                      name="state-miss-policy"
                      value={option.value}
                      checked={(fwd.status?.stateMissPolicy ?? "preserve") === option.value}
                      aria-label={option.label}
                      aria-description={option.tooltip}
                      disabled={fwd.busy !== null}
                      onChange={() => void fwd.setStateMissPolicy(option.value)}
                    />
                    <span className="state-policy-option__radio" aria-hidden="true" />
                    <span className="state-policy-option__copy">
                      <strong>{option.label}</strong>
                      <small>{option.summary}</small>
                    </span>
                  </label>
                ))}
              </div>
            </div>
          </div>
        </div>

        <footer className="page-footer">
          <div className="page-footer__tools">
            <span><Shield size={13} /> 本地运行 · 配置尽在掌握</span>
            <button ref={networkLogTriggerRef} className="footer-tool" type="button" onClick={() => setNetworkLogsOpen(true)}>
              <Activity size={13} />网络日志<span className="footer-tool__count">{fwd.status.logs.length}</span>
            </button>
          </div>
          <span>CODEX STATE KIT</span>
        </footer>
        <NetworkLogDialog
          open={networkLogsOpen}
          status={fwd.status}
          triggerRef={networkLogTriggerRef}
          onClose={() => setNetworkLogsOpen(false)}
        />
      </div>
    </AppShell>
  );
}
