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
import Waypoints from "lucide-react/dist/esm/icons/waypoints.js";
import Pause from "lucide-react/dist/esm/icons/pause.js";
import Play from "lucide-react/dist/esm/icons/play.js";
import RefreshCw from "lucide-react/dist/esm/icons/refresh-cw.js";
import { AppShell } from "@/components/AppShell";
import { NetworkLogDialog } from "@/components/NetworkLogDialog";
import { WarpPanel } from "@/components/WarpPanel";
import { useCodexStateKit } from "@/hooks/useCodexStateKit";
import { isTauri } from "@/lib/api";
import type { LoginMode, Status, TurnStateView, StateMissPolicy, TokenReusePolicy, NetworkRoutePolicy, LatencySample } from "@/types";

function chipLabel(status: Status) {
  if (status.attached) return "已接入";
  return status.proxyOk ? "代理已开" : "代理未开";
}

function chipClass(status: Status) {
  if (status.attached) return "runtime-chip runtime-chip--accent";
  return status.proxyOk ? "runtime-chip" : "runtime-chip runtime-chip--down";
}

function delayText(sample?: LatencySample | null): string | null {
  if (!sample) return null;
  if (sample.delayMs != null) return `${sample.delayMs} ms`;
  return sample.error || "超时";
}

function tokenChip(view?: TurnStateView | null, paused = false) {
  if (paused && view?.status !== "active") return { label: "已暂停获取", className: "runtime-chip runtime-chip--idle" };
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
    tooltip: "已带 State 则替换为当前账号的有效凭证。新一轮首包未带时补上规范票据并包装成探针同轮第二包。同轮续跑只改请求头，保留 previous_response_id。没有匹配凭证时保留客户端原值。",
  },
  {
    value: "wait",
    label: "无票等待",
    summary: "等有效票再发送",
    tooltip: "缺少规范票据时持续等待再写入。新一轮首包包装成同轮第二包；同轮续跑只改请求头。请求账号与当前登录不同时立即报错，不转发。同一账号的旧 Access Token 仍会覆盖后继续。客户端取消，或账号、线路、策略变化时终止。",
  },
  {
    value: "strip",
    label: "无票剥离",
    summary: "无票时删除",
    tooltip: "已带则替换。新一轮首包未带则补上规范票据并包装成探针同轮第二包；同轮续跑只改请求头。没有匹配凭证时删除客户端 State，让上游自行处理。",
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

function tokenCopy(view?: TurnStateView | null, fetchError?: string | null, reusePolicy: TokenReusePolicy = "shared_292", paused = false) {
  const bound = view?.boundTokenLen ?? 292;
  if (paused) {
    if (view?.status === "active") {
      const summary = modelSummary(view);
      return {
        title: reusePolicy === "shared_292" && bound === 292 ? "292 Token 正在跨模型复用" : `${bound} Token 正在复用`,
        body: summary ? `已暂停后台获取 · ${summary}` : "已暂停后台获取，已缓存的 Token 仍可注入。",
        loading: false,
      };
    }
    return {
      title: `已暂停获取 ${bound} Token`,
      body: "后台不再打票。已缓存的 Token 仍可注入，需要时再点继续获取。",
      loading: false,
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
      title: reusePolicy === "shared_292" && bound === 292 ? "292 Token 正在跨模型复用" : `${bound} Token 正在复用`,
      body: reusePolicy === "shared_292" && bound === 292 && view.sharedSourceModel
        ? `同账号共享 · 来源 ${view.sharedSourceModel} · ${summary}`
        : summary,
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
    body: "凭据包已超过 240 秒，正在后台再采一张。",
    loading: true,
  };
}

export default function App() {
  const fwd = useCodexStateKit();
  const [codexHome, setCodexHome] = useState("");
  const [outboundProxy, setOutboundProxy] = useState("");
  const [upstreamProxy, setUpstreamProxy] = useState("");
  const [mihomoSubscription, setMihomoSubscription] = useState("");
  const [mihomoNode, setMihomoNode] = useState("");
  const [forcedModel, setForcedModel] = useState("");
  const [stateFetchModel, setStateFetchModel] = useState("");
  const [loginMode, setLoginMode] = useState<LoginMode>("browser");
  const [refreshTokenInput, setRefreshTokenInput] = useState("");
  const [accessTokenInput, setAccessTokenInput] = useState("");
  const [networkLogsOpen, setNetworkLogsOpen] = useState(false);
  const networkLogTriggerRef = useRef<HTMLButtonElement>(null);
  const hydrated = useRef(false);

  useEffect(() => {
    if (!fwd.status || hydrated.current) return;
    hydrated.current = true;
    setCodexHome(fwd.status.codexHome);
    setOutboundProxy(fwd.status.outboundProxy ?? "");
    setUpstreamProxy(fwd.status.upstreamProxy ?? "");
    setMihomoSubscription(fwd.status.mihomoSubscription ?? "");
    setMihomoNode(fwd.status.mihomoNode ?? "");
    setForcedModel(fwd.status.forcedModel ?? "");
    setStateFetchModel(fwd.status.stateFetchModel ?? "");
  }, [fwd.status]);


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

  const loggedIn = Boolean(fwd.login?.loggedIn);
  const loginLabel = fwd.login?.email || "ChatGPT";
  const loginMeta = [
    fwd.login?.accountId,
    fwd.login?.refreshable
      ? "含 Refresh Token"
      : fwd.login?.authMode === "chatgptAuthTokens"
        ? "Access Token · 不可自动刷新"
        : null,
  ].filter(Boolean).join(" · ");
  const turn = fwd.status.turnState;
  const stateDonor = fwd.status.tokenReusePolicy === "shared_292" ? fwd.status.stateFetchModel : "";
  const fetchPaused = Boolean(fwd.status.tokenFetchPaused);
  const token = tokenChip(turn, fetchPaused);
  const degrade = degradeChip(fwd.status);
  const copy = tokenCopy(
    turn,
    fwd.status.fetchError ?? (fwd.status.outboundMode === "warp" ? fwd.status.warp.error : fwd.status.outboundMode === "mihomo" ? fwd.status.mihomo?.error : null),
    fwd.status.tokenReusePolicy,
    fetchPaused,
  );
  const age = formatAge(turn?.ageSecs);
  const sourceLabel =
    turn?.source === "fetch" ? "StateKit 获取" : turn?.source === "ws" ? "WebSocket" : turn?.source === "http" ? "HTTP" : null;
  const meta = [age, turn?.len ? `${turn.len} 字节` : null, sourceLabel, turn?.boundProxySession ? `session ${turn.boundProxySession}` : null].filter(Boolean).join(" · ");
  const selectedNode = mihomoNode || fwd.status.mihomo?.selected || "";
  const selectedNodeDelay = fwd.latency.mihomo?.samples.find((item) => item.name === selectedNode);

  const copyCode = async () => {
    if (!fwd.device?.userCode) return;
    try {
      await navigator.clipboard.writeText(fwd.device.userCode);
    } catch {
      // ignore
    }
  };

  const submitRefreshToken = async () => {
    const refreshToken = refreshTokenInput.trim();
    if (!refreshToken) return;
    if (await fwd.importRefreshLogin(codexHome, refreshToken)) {
      setRefreshTokenInput("");
    }
  };

  const submitAccessToken = async () => {
    const accessToken = accessTokenInput.trim();
    if (!accessToken) return;
    if (await fwd.importAccessLogin(codexHome, accessToken)) {
      setAccessTokenInput("");
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
            <button
              ref={networkLogTriggerRef}
              className="network-log-trigger"
              type="button"
              onClick={() => setNetworkLogsOpen(true)}
            >
              <Activity size={13} />
              网络日志
              <span className="network-log-trigger__count">{fwd.status.logs.length}</span>
            </button>
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
              <strong>{copy.title}</strong>
              {copy.body ? <small>{copy.body}</small> : null}
              {meta ? <p className="token-card__meta">{meta}</p> : null}
            </div>
          </div>
          <div className="token-card__actions">
            <button
              type="button"
              className="token-fetch-toggle"
              disabled={fwd.busy !== null}
              title="立即重新获取 Token，绕过后台冷却；暂停自动获取时也可点一次"
              onClick={() => void fwd.refetchTurnState()}
            >
              <RefreshCw size={13} className={fwd.busy === "refresh" ? "is-spinning" : undefined} />
              {fwd.busy === "refresh" ? "正在获取" : "重新获取"}
            </button>
            <button
              type="button"
              className={`token-fetch-toggle${fetchPaused ? " token-fetch-toggle--paused" : ""}`}
              aria-pressed={fetchPaused}
              disabled={fwd.busy !== null}
              title={fetchPaused ? "继续后台获取 Token" : "暂停后台打票，已缓存的 Token 仍可注入"}
              onClick={() => void fwd.setTokenFetchPaused(!fetchPaused)}
            >
              {fetchPaused ? <Play size={13} /> : <Pause size={13} />}
              {fetchPaused ? "继续获取" : "暂停获取"}
            </button>
            <span className="token-card__badge"><Radio size={14} /> {fetchPaused ? `已暂停 · 绑定 ${turn?.boundTokenLen ?? 292}` : fwd.status.proxyOk ? `自动管理 · 绑定 ${turn?.boundTokenLen ?? 292}` : "等待代理启动"}</span>
          </div>
          <div className="token-reuse-policy" role="group" aria-labelledby="token-reuse-policy-label">
            <div className="token-reuse-policy__heading">
              <strong id="token-reuse-policy-label">Token 复用策略</strong>
              <span>自动保存 · 即时生效</span>
            </div>
            <div className="token-reuse-policy__options">
              <label className="token-reuse-option">
                <input type="radio" name="token-reuse-policy" value="shared_292"
                  checked={fwd.status.tokenReusePolicy === "shared_292"} disabled={fwd.busy !== null}
                  onChange={() => void fwd.setTokenReusePolicy("shared_292")} />
                <span><strong>跨模型复用 292 <small>默认</small></strong><span>一张有效票据同账号共用，不再逐模型打票</span></span>
              </label>
              <label className="token-reuse-option">
                <input type="radio" name="token-reuse-policy" value="per_model"
                  checked={fwd.status.tokenReusePolicy === "per_model"} disabled={fwd.busy !== null}
                  onChange={() => void fwd.setTokenReusePolicy("per_model")} />
                <span><strong>按模型独立 <small>旧策略</small></strong><span>各模型分别获取，只复用各自的 Token</span></span>
              </label>
            </div>
            {fwd.status.tokenReusePolicy === "shared_292" ? (
              <label className="field token-reuse-donor">
                <span>取 State 的模型</span>
                <input
                  spellCheck={false}
                  autoComplete="off"
                  disabled={fwd.busy !== null}
                  value={stateFetchModel}
                  placeholder="例如 gpt-5.5，留空则从已有模型里选一个"
                  onChange={(event) => setStateFetchModel(event.target.value)}
                  onBlur={(event) => {
                    const value = event.currentTarget.value;
                    if (value.trim() === (fwd.status?.stateFetchModel ?? "")) return;
                    setStateFetchModel(value);
                    void fwd.saveStateFetchModel(value);
                  }}
                  onKeyDown={(event) => {
                    if (event.key === "Enter") event.currentTarget.blur();
                  }}
                />
              </label>
            ) : null}
            <p>292 与线路 Cookie 成套保存、成套注入。填写取票模型后，跨模型复用只向这个模型索取 292，其他模型不再打这张共享票。332 及其他绑定长度仍按模型独立。留空则从已有模型里选一个供体。</p>
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
                    {m.sharedFromModel ? <span className="token-model__shared" title={`票据来源：${m.sharedFromModel}`}>共享 · {m.sharedFromModel}</span> : null}
                    {stateDonor && stateDonor === m.model ? <span className="token-model__donor">取票</span> : null}
                    {hasOverride ? <span className="token-model__override">独立绑定 {effectiveBound}</span> : null}
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
              检测到 312 降智信号{fwd.status.degradedAt ? `（${fwd.status.degradedAt}）` : ""}，{fetchPaused ? "已暂停获取，继续获取后才会重新打票。" : `正在通过出站代理重新采集 ${turn?.boundTokenLen ?? 292} token…`}
            </span>
          </div>
        ) : null}

        <div className="panel dash-grid">
        <section className="connection-section panel--proxy">
          <header>
            <div className="section-heading"><span className="section-icon"><Network size={19} /></span><div><h2>出站网络</h2><p>{(fwd.status.networkRoutePolicy ?? "same_network") === "same_network" ? "获取 292 与业务发送使用同一条线路" : "Token 获取与业务转发可分开配置"}</p></div></div>
            <span className="section-step">01</span>
          </header>
          <div className="token-reuse-policy network-route-policy" role="group" aria-labelledby="network-route-policy-label">
            <div className="token-reuse-policy__heading">
              <strong id="network-route-policy-label">网络策略</strong>
              <span>自动保存 · 即时生效</span>
            </div>
            <div className="token-reuse-policy__options">
              <label className="token-reuse-option">
                <input type="radio" name="network-route-policy" value="same_network"
                  checked={(fwd.status.networkRoutePolicy ?? "same_network") === "same_network"} disabled={fwd.busy !== null}
                  onChange={() => void fwd.setNetworkRoutePolicy("same_network" as NetworkRoutePolicy)} />
                <span><strong>同网获取并发送 <small>默认</small></strong><span>用当前代理打 292，拿到后业务请求走同一条线路</span></span>
              </label>
              <label className="token-reuse-option">
                <input type="radio" name="network-route-policy" value="separate"
                  checked={fwd.status.networkRoutePolicy === "separate"} disabled={fwd.busy !== null}
                  onChange={() => void fwd.setNetworkRoutePolicy("separate")} />
                <span><strong>Token 与业务分路 <small>旧</small></strong><span>Token 走获取代理，业务单独走上游转发代理</span></span>
              </label>
            </div>
          </div>
          <div className="proxy-mode" role="group" aria-label="出站代理模式">
            <button type="button" aria-pressed={fwd.status.outboundMode === "warp"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveSettings(codexHome, outboundProxy, "warp")}><Cloud size={15} />内置 WARP</button>
            <button type="button" aria-pressed={fwd.status.outboundMode === "manual"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveSettings(codexHome, outboundProxy, "manual")}><Network size={14} />手动代理</button>
            <button type="button" aria-pressed={fwd.status.outboundMode === "mihomo"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveMihomo(mihomoSubscription, mihomoNode)}><Waypoints size={14} />订阅节点</button>
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
              placeholder="socks5://user-region-DE-sid-{session}-t-120:pass@host:3010"
              onChange={(event) => setOutboundProxy(event.target.value)}
              onBlur={() => void fwd.saveSettings(codexHome, outboundProxy)}
              onKeyDown={(event) => {
                if (event.key === "Enter") void fwd.saveSettings(codexHome, outboundProxy);
              }}
            />
          </label>
          <p className="panel__hint">支持 socks5 / socks5h / http，离开输入框后自动保存。可把出口写成 {'{session}'}，打票时自动轮换；拿到稳定 292 后绑定该 session 发业务。{(fwd.status.networkRoutePolicy ?? "same_network") === "same_network" ? " 同网策略下，业务转发也使用绑定后的同一条线路。" : ""}</p>
          <div className="latency-row">
            <button type="button" className="token-fetch-toggle" disabled={fwd.probing !== null} onClick={() => void fwd.probeLatency("manual", outboundProxy)}>
              {fwd.probing === "manual" ? "测试中" : "测延迟"}
            </button>
            {delayText(fwd.latency.manual?.samples[0]) ? <span className={fwd.latency.manual?.samples[0]?.delayMs != null ? "latency-row__ok" : "latency-row__bad"}>{delayText(fwd.latency.manual?.samples[0])}</span> : null}
          </div>
          </> : fwd.status.outboundMode === "mihomo" ? (
          <div className="warp-panel">
            <label className="field">
              <span>订阅地址</span>
              <input
                type="text"
                spellCheck={false}
                autoComplete="off"
                disabled={fwd.busy !== null}
                value={mihomoSubscription}
                placeholder="https://example.com/sub 或本地文件、分享链接"
                onChange={(event) => setMihomoSubscription(event.target.value)}
                onBlur={() => void fwd.saveMihomo(mihomoSubscription, mihomoNode)}
                onKeyDown={(event) => {
                  if (event.key === "Enter") void fwd.saveMihomo(mihomoSubscription, mihomoNode);
                }}
              />
            </label>
            <label className="field">
              <span>当前节点</span>
              <select
                disabled={fwd.busy !== null || (fwd.status.mihomo?.nodes.length ?? 0) === 0}
                value={mihomoNode || fwd.status.mihomo?.selected || ""}
                onChange={(event) => {
                  setMihomoNode(event.target.value);
                  void fwd.saveMihomo(mihomoSubscription, event.target.value);
                }}
              >
                {(fwd.status.mihomo?.nodes.length ?? 0) === 0 ? <option value="">连接后列出节点</option> : null}
                {(fwd.status.mihomo?.nodes ?? []).map((node) => {
                  const sample = fwd.latency.mihomo?.samples.find((item) => item.name === node);
                  const mark = sample ? (sample.delayMs != null ? ` · ${sample.delayMs} ms` : " · 超时") : "";
                  return <option key={node} value={node}>{node}{mark}</option>;
                })}
              </select>
            </label>
            <div className="latency-row">
              <button type="button" className="token-fetch-toggle" disabled={fwd.probing !== null || (isTauri && fwd.status.mihomo?.phase !== "connected")} onClick={() => void fwd.probeLatency("mihomo")}>
                {fwd.probing === "mihomo" ? "测试中" : "测延迟"}
              </button>
              {delayText(selectedNodeDelay) ? (
                <span className={selectedNodeDelay?.delayMs != null ? "latency-row__ok" : "latency-row__bad"}>
                  {delayText(selectedNodeDelay)}
                </span>
              ) : null}
            </div>
            {fwd.status.mihomo?.error ? <p className="warp-error">{fwd.status.mihomo.error}</p> : null}
            <p className="panel__hint">
              {fwd.status.mihomo?.phase === "connected"
                ? `已连接${fwd.status.mihomo.selected ? ` · ${fwd.status.mihomo.selected}` : ""}${fwd.status.mihomo.proxyUrl ? ` · ${fwd.status.mihomo.proxyUrl}` : ""}`
                : "内核随应用内置。同网时打票和业务都走选中节点；分路时打票走订阅节点，业务走上游转发代理并复验。"}
            </p>
          </div>
          ) : <WarpPanel status={fwd.status.warp} probing={fwd.probing === "warp"} delayText={delayText(fwd.latency.warp?.samples[0])} onProbe={() => void fwd.probeLatency("warp")} onTerms={() => void fwd.openWarpTerms()} />}
          {(fwd.status.networkRoutePolicy ?? "same_network") === "same_network" ? (
            <p className="panel__hint">业务请求会复用上面这条出站线路，不再单独填写上游转发代理。</p>
          ) : <>
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
          </>}
        </section>

        <section className="connection-section">
          <header>
            <div className="section-heading"><span className="section-icon section-icon--warm"><Terminal size={19} /></span><div><h2>Codex 接入</h2><p>登录账号，连接你的客户端</p></div></div>
            <span className="section-step">02</span>
          </header>
          <div className="proxy-mode login-methods" role="group" aria-label="登录方式">
            <button type="button" aria-pressed={loginMode === "browser"} disabled={fwd.busy !== null || Boolean(fwd.device)} onClick={() => setLoginMode("browser")}><ExternalLink size={14} />浏览器回调</button>
            <button type="button" aria-pressed={loginMode === "device"} disabled={fwd.busy !== null || Boolean(fwd.device)} onClick={() => setLoginMode("device")}><Copy size={14} />授权码登录</button>
            <button type="button" aria-pressed={loginMode === "refresh"} disabled={fwd.busy !== null || Boolean(fwd.device)} onClick={() => setLoginMode("refresh")}><Radio size={14} />Refresh Token</button>
            <button type="button" aria-pressed={loginMode === "access"} disabled={fwd.busy !== null || Boolean(fwd.device)} onClick={() => setLoginMode("access")}><Shield size={14} />Access Token</button>
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
            ) : loginMode === "refresh" ? (
              <div className="credential-import">
                <label className="field">
                  <span>Refresh Token</span>
                  <input
                    type="password"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={fwd.busy !== null}
                    value={refreshTokenInput}
                    placeholder="粘贴 Refresh Token"
                    onChange={(event) => setRefreshTokenInput(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") void submitRefreshToken();
                    }}
                  />
                </label>
                <p>会先向官方授权服务换取新凭据，并保存服务端返回的轮换 Refresh Token。</p>
                <div className="panel__actions">
                  <button
                    className="button button--primary"
                    type="button"
                    disabled={fwd.busy !== null || !refreshTokenInput.trim()}
                    onClick={() => void submitRefreshToken()}
                  >
                    {fwd.busy === "login" ? <span className="spinner" /> : <LogIn size={14} />}
                    导入并登录
                  </button>
                </div>
              </div>
            ) : loginMode === "access" ? (
              <div className="credential-import">
                <label className="field">
                  <span>Access Token</span>
                  <input
                    type="password"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={fwd.busy !== null}
                    value={accessTokenInput}
                    placeholder="粘贴 Codex Access Token（JWT）"
                    onChange={(event) => setAccessTokenInput(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") void submitAccessToken();
                    }}
                  />
                </label>
                <p className="credential-warning">Access Token 不可自动刷新；过期后需要重新导入。</p>
                <div className="panel__actions">
                  <button
                    className="button button--primary"
                    type="button"
                    disabled={fwd.busy !== null || !accessTokenInput.trim()}
                    onClick={() => void submitAccessToken()}
                  >
                    {fwd.busy === "login" ? <span className="spinner" /> : <LogIn size={14} />}
                    导入并登录
                  </button>
                </div>
              </div>
            ) : loggedIn ? (
              <div className="login-current">
                <div>
                  <strong>{loginLabel}</strong>
                  {loginMeta ? <span className="login-meta">{loginMeta}</span> : null}
                </div>
                <button
                  className="button button--ghost"
                  type="button"
                  disabled={fwd.busy !== null}
                  onClick={() => void fwd.startLogin(codexHome, loginMode)}
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
                  onClick={() => void fwd.startLogin(codexHome, loginMode)}
                >
                  {fwd.busy === "login" ? <span className="spinner" /> : <LogIn size={14} />}
                  登录 ChatGPT
                </button>
              </div>
            )}
          </div>
          <p className="panel__hint">
            浏览器与授权码走官方 OAuth；Refresh Token 会换票并保存轮换凭据；Access Token 以 Codex 外部 Token 模式接入，过期后需重新导入。凭据提交后不回显、不写日志。 启动后自动接入，关闭时还原原来的官方账号、路由和本地模型配置。
          </p>
        </section>
          <label className="field connection-directory">
            <span>Codex 工作目录</span>
            <input spellCheck={false} disabled={fwd.busy !== null} value={codexHome} onChange={(event) => setCodexHome(event.target.value)}
              onBlur={() => void fwd.saveSettings(codexHome, outboundProxy)}
              onKeyDown={(event) => { if (event.key === "Enter") event.currentTarget.blur(); }} />
          </label>
          <label className="field connection-directory">
            <span>强制绑定模型</span>
            <input
              spellCheck={false}
              autoComplete="off"
              disabled={fwd.busy !== null}
              value={forcedModel}
              placeholder="例如 gpt-6-astra，留空按下游原模型转发"
              onChange={(event) => setForcedModel(event.target.value)}
              onBlur={() => void fwd.saveForcedModel(forcedModel)}
              onKeyDown={(event) => {
                if (event.key === "Enter") event.currentTarget.blur();
              }}
            />
          </label>
          <p className="panel__hint">填写后，下游无论请求什么模型 ID，都会改成这个值再转发给上游，Token 也按该模型获取和复用。</p>
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
          <span><Shield size={13} /> 本地运行 · 配置尽在掌握</span>
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
