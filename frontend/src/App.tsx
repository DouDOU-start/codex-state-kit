import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import Copy from "lucide-react/dist/esm/icons/copy.js";
import ExternalLink from "lucide-react/dist/esm/icons/external-link.js";
import LogIn from "lucide-react/dist/esm/icons/log-in.js";
import Shield from "lucide-react/dist/esm/icons/shield.js";
import TriangleAlert from "lucide-react/dist/esm/icons/triangle-alert.js";
import Activity from "lucide-react/dist/esm/icons/activity.js";
import Monitor from "lucide-react/dist/esm/icons/monitor.js";
import Network from "lucide-react/dist/esm/icons/network.js";
import Terminal from "lucide-react/dist/esm/icons/terminal.js";
import CircleCheck from "lucide-react/dist/esm/icons/circle-check.js";
import Radio from "lucide-react/dist/esm/icons/radio.js";
import Waypoints from "lucide-react/dist/esm/icons/waypoints.js";
import LayoutDashboard from "lucide-react/dist/esm/icons/layout-dashboard.js";
import Settings2 from "lucide-react/dist/esm/icons/settings-2.js";
import ScrollText from "lucide-react/dist/esm/icons/scroll-text.js";
import BadgeDollarSign from "lucide-react/dist/esm/icons/badge-dollar-sign.js";
import { AppShell } from "@/components/AppShell";
import { BillingPanel } from "@/components/BillingPanel";
import { MihomoGroupPanel } from "@/components/MihomoGroupPanel";
import { UsageRecordsPanel } from "@/components/UsageRecordsPanel";
import { PricingPanel } from "@/components/PricingPanel";
import { useCodexStateKit } from "@/hooks/useCodexStateKit";
import { isTauri } from "@/lib/api";
import type { LoginMode, Status, LatencySample, VmIdentityView } from "@/types";

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

function degradeChip(status: Status) {
  if (!status.degraded) return null;
  return { label: "312 降智", className: "runtime-chip runtime-chip--down" };
}

type TabId = "overview" | "records" | "pricing" | "network" | "account" | "device";

const TABS: { id: TabId; label: string; Icon: typeof Activity }[] = [
  { id: "overview", label: "概览", Icon: LayoutDashboard },
  { id: "records", label: "使用记录", Icon: ScrollText },
  { id: "pricing", label: "模型价格", Icon: BadgeDollarSign },
  { id: "network", label: "出站网络", Icon: Network },
  { id: "account", label: "Codex 接入", Icon: Terminal },
  { id: "device", label: "虚拟设备", Icon: Monitor },
];

const TAB_STORAGE_KEY = "codex-state-kit.tab";

function initialTab(): TabId {
  try {
    const saved = window.localStorage.getItem(TAB_STORAGE_KEY);
    if (TABS.some((tab) => tab.id === saved)) return saved as TabId;
  } catch {
    // ignore
  }
  return "overview";
}

export default function App() {
  const fwd = useCodexStateKit();
  const [codexHome, setCodexHome] = useState("");
  const [outboundProxy, setOutboundProxy] = useState("");
  const [mihomoSubscription, setMihomoSubscription] = useState("");
  const [mihomoNode, setMihomoNode] = useState("");
  const [forcedModel, setForcedModel] = useState("");
  const [cliVersion, setCliVersion] = useState("0.155.0");
  const [vmOriginator, setVmOriginator] = useState("codex_cli_rs");
  const [osType, setOsType] = useState("Mac OS");
  const [osVersion, setOsVersion] = useState("15.5.0");
  const [vmArch, setVmArch] = useState("arm64");
  const [vmTerminal, setVmTerminal] = useState("xterm-256color");
  const [loginMode, setLoginMode] = useState<LoginMode>("browser");
  const [refreshTokenInput, setRefreshTokenInput] = useState("");
  const [accessTokenInput, setAccessTokenInput] = useState("");
  const [tab, setTab] = useState<TabId>(initialTab);
  const tabRefs = useRef<Partial<Record<TabId, HTMLButtonElement | null>>>({});
  const hydrated = useRef(false);

  useEffect(() => {
    if (!fwd.status || hydrated.current) return;
    hydrated.current = true;
    setCodexHome(fwd.status.codexHome);
    setOutboundProxy(fwd.status.outboundProxy ?? "");
    setMihomoSubscription(fwd.status.mihomoSubscription ?? "");
    setMihomoNode(fwd.status.mihomoNode ?? "");
    setForcedModel(fwd.status.forcedModel ?? "");
    const identity = fwd.status.vmIdentity;
    if (identity) {
      setCliVersion(identity.cliVersion);
      setVmOriginator(identity.originator);
      setOsType(identity.osType);
      setOsVersion(identity.osVersion);
      setVmArch(identity.arch);
      setVmTerminal(identity.terminal);
    }
  }, [fwd.status]);

  function selectTab(next: TabId, focus = false) {
    setTab(next);
    if (focus) tabRefs.current[next]?.focus();
    try {
      window.localStorage.setItem(TAB_STORAGE_KEY, next);
    } catch {
      // ignore
    }
  }

  function onTabKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    const focused = TABS.findIndex((item) => tabRefs.current[item.id] === document.activeElement);
    const index = focused === -1 ? TABS.findIndex((item) => item.id === tab) : focused;
    const last = TABS.length - 1;
    const next =
      event.key === "ArrowRight" ? (index === last ? 0 : index + 1)
        : event.key === "ArrowLeft" ? (index === 0 ? last : index - 1)
          : event.key === "Home" ? 0
            : event.key === "End" ? last
              : null;
    if (next === null) return;
    event.preventDefault();
    selectTab(TABS[next].id, true);
  }

  function applyVmDraft(identity: VmIdentityView) {
    setCliVersion(identity.cliVersion);
    setVmOriginator(identity.originator);
    setOsType(identity.osType);
    setOsVersion(identity.osVersion);
    setVmArch(identity.arch);
    setVmTerminal(identity.terminal);
  }

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
  const degrade = degradeChip(fwd.status);
  const tabAlert: Partial<Record<TabId, string>> = {
    network: fwd.status.proxyError || fwd.status.mihomo?.error ? "出站网络异常" : undefined,
    account: loggedIn ? undefined : "尚未登录",
  };
  const selectedNode = mihomoNode || fwd.status.mihomo?.selected || "";
  const selectedNodeDelay = fwd.latency.mihomo?.samples.find((item) => item.name === selectedNode);
  const mihomoGroups = fwd.status?.mihomo.groups ?? [];
  const kitGroups = mihomoGroups.filter((group) => group.name === "Kit");
  const shownMihomoGroups = kitGroups.length > 0
    ? kitGroups
    : mihomoGroups.filter((group) => group.groupType === "select");

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
        <div className="page-nav">
          <div className="page-tabs" role="tablist" aria-label="页面分区" onKeyDown={onTabKeyDown}>
            {TABS.map(({ id, label, Icon }) => (
              <button
                key={id}
                ref={(node) => { tabRefs.current[id] = node; }}
                id={`tab-${id}`}
                type="button"
                role="tab"
                aria-selected={tab === id}
                aria-controls={`tabpanel-${id}`}
                tabIndex={tab === id ? 0 : -1}
                onClick={() => selectTab(id)}
              >
                <Icon size={14} aria-hidden="true" />
                {label}
                {tabAlert[id] ? <i className="page-tabs__alert" title={tabAlert[id]} aria-label={tabAlert[id]} /> : null}
              </button>
            ))}
          </div>
          <div className="page-actions">
            <span className={chipClass(fwd.status)}>
              <i />
              {chipLabel(fwd.status)}
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

        {fwd.status.degraded ? (
          <div className="banner banner--error" role="alert">
            <span>
              <TriangleAlert size={14} style={{ verticalAlign: "middle", marginRight: 4 }} />
              检测到 312 降智信号{fwd.status.degradedAt ? `（${fwd.status.degradedAt}）` : ""}。
            </span>
          </div>
        ) : null}


        <div className="tab-panel" role="tabpanel" id="tabpanel-overview" aria-labelledby="tab-overview" hidden={tab !== "overview"}>
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

        <BillingPanel
          currentAccountId={fwd.status.currentAccountId}
          currentAccountEmail={fwd.status.currentAccountEmail}
        />
        </div>


        <section className="panel tab-panel tab-panel--flush" role="tabpanel" id="tabpanel-records" aria-labelledby="tab-records" hidden={tab !== "records"}>
          <UsageRecordsPanel active={tab === "records"} status={fwd.status} />
        </section>

        <section className="panel tab-panel tab-panel--flush" role="tabpanel" id="tabpanel-pricing" aria-labelledby="tab-pricing" hidden={tab !== "pricing"}>
          <PricingPanel active={tab === "pricing"} />
        </section>

        <section className="panel tab-panel" role="tabpanel" id="tabpanel-network" aria-labelledby="tab-network" hidden={tab !== "network"}>
          <header>
            <div className="section-heading"><span className="section-icon"><Network size={19} /></span><div><h2>出站网络</h2><p>获取 Token 与业务发送共用这一条出站线路</p></div></div>
          </header>
          <div className="proxy-mode" role="group" aria-label="出站代理模式">
            <button type="button" aria-pressed={fwd.status.outboundMode === "manual"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveSettings(codexHome, outboundProxy, "manual")}><Network size={14} />手动代理</button>
            <button type="button" aria-pressed={fwd.status.outboundMode === "mihomo"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveMihomo(mihomoSubscription, mihomoNode)}><Waypoints size={14} />订阅节点</button>
          </div>
          <div className="ws-line">
            <label>
              <input
                type="checkbox"
                checked={fwd.status.wsUpstreamEnabled !== false}
                disabled={fwd.busy !== null}
                onChange={(event) => void fwd.setWsUpstreamEnabled(event.target.checked)}
              />
              上游走 WebSocket
            </label>
            <span>{fwd.status.wsUpstreamConnected ? `已连接${fwd.status.wsUpstreamConnectedAt ? ` · ${fwd.status.wsUpstreamConnectedAt}` : ""}` : "未连接"}</span>
            <button type="button" className="text-button" disabled={fwd.busy !== null} onClick={() => void fwd.reconnectUpstream()}>重连</button>
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
          <p className="panel__hint">支持 socks5 / socks5h / http，离开输入框后自动保存。可把出口写成 {'{session}'}，打票时自动轮换；拿到稳定 292 后绑定该 session，业务也走同一条线路。</p>
          <div className="latency-row">
            <button type="button" className="token-fetch-toggle" disabled={fwd.probing !== null} onClick={() => void fwd.probeLatency("manual", outboundProxy)}>
              {fwd.probing === "manual" ? "测试中" : "测延迟"}
            </button>
            {delayText(fwd.latency.manual?.samples[0]) ? <span className={fwd.latency.manual?.samples[0]?.delayMs != null ? "latency-row__ok" : "latency-row__bad"}>{delayText(fwd.latency.manual?.samples[0])}</span> : null}
          </div>
          </> : (
          <div className="mihomo-panel">
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
            {shownMihomoGroups.map((group) => (
              <MihomoGroupPanel
                key={group.name}
                group={group}
                probing={fwd.probingGroup === group.name || fwd.probingGroup === "*"}
                onSelect={(node) => void fwd.selectMihomoNode(group.name, node)}
                onProbe={() => void fwd.probeMihomoGroup(group.name)}
              />
            ))}
            {shownMihomoGroups.length > 0 ? null : (
              <>
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
              </>
            )}
            {fwd.status.mihomo?.error ? <p className="mihomo-error">{fwd.status.mihomo.error}</p> : null}
            <p className="panel__hint">
              {fwd.status.mihomo?.phase === "connected"
                ? `已连接${fwd.status.mihomo.selected ? ` · ${fwd.status.mihomo.selected}` : ""}${fwd.status.mihomo.proxyUrl ? ` · ${fwd.status.mihomo.proxyUrl}` : ""}`
                : "内核随应用内置。打票和业务都走这条订阅线路。"}
            </p>
          </div>
          )}
        </section>

        <div className="tab-panel tab-panel--split" role="tabpanel" id="tabpanel-account" aria-labelledby="tab-account" hidden={tab !== "account"}>
        <section className="panel">
          <header>
            <div className="section-heading"><span className="section-icon section-icon--warm"><Terminal size={19} /></span><div><h2>Codex 接入</h2><p>登录账号，连接你的客户端</p></div></div>
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
        <section className="panel">
          <header>
            <div className="section-heading"><span className="section-icon"><Settings2 size={19} /></span><div><h2>转发设置</h2><p>本机 Codex 目录与上游模型</p></div></div>
          </header>
          <label className="field">
            <span>Codex 工作目录</span>
            <input spellCheck={false} disabled={fwd.busy !== null} value={codexHome} onChange={(event) => setCodexHome(event.target.value)}
              onBlur={() => void fwd.saveSettings(codexHome, outboundProxy)}
              onKeyDown={(event) => { if (event.key === "Enter") event.currentTarget.blur(); }} />
          </label>
          <label className="field">
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
        </section>
        </div>

        <section className="panel vm-panel tab-panel" role="tabpanel" id="tabpanel-device" aria-labelledby="tab-device" hidden={tab !== "device"}>
          <header>
            <div className="section-heading">
              <span className="section-icon"><Monitor size={19} /></span>
              <div>
                <h2>虚拟设备</h2>
                <p>{fwd.status.vmIdentity?.userAgent ?? "上游看到的 Codex CLI 身份"}</p>
              </div>
            </div>
            <span className="vm-identity__id">Installation {fwd.status.vmIdentity?.installationId ?? "—"}</span>
          </header>
          <div className="vm-identity__grid">
            <label className="field">
              <span>CLI 版本</span>
              <input type="text" spellCheck={false} autoComplete="off" disabled={fwd.busy !== null} value={cliVersion} onChange={(event) => setCliVersion(event.target.value)} />
            </label>
            <label className="field">
              <span>Originator</span>
              <input type="text" spellCheck={false} autoComplete="off" disabled={fwd.busy !== null} value={vmOriginator} onChange={(event) => setVmOriginator(event.target.value)} />
            </label>
            <label className="field">
              <span>系统</span>
              <select disabled={fwd.busy !== null} value={osType} onChange={(event) => setOsType(event.target.value)}>
                <option value="Mac OS">Mac OS</option>
                <option value="Linux">Linux</option>
                <option value="Windows">Windows</option>
              </select>
            </label>
            <label className="field">
              <span>系统版本</span>
              <input type="text" spellCheck={false} autoComplete="off" disabled={fwd.busy !== null} value={osVersion} onChange={(event) => setOsVersion(event.target.value)} />
            </label>
            <label className="field">
              <span>架构</span>
              <select disabled={fwd.busy !== null} value={vmArch} onChange={(event) => setVmArch(event.target.value)}>
                <option value="arm64">arm64</option>
                <option value="x86_64">x86_64</option>
              </select>
            </label>
            <label className="field">
              <span>终端</span>
              <input type="text" spellCheck={false} autoComplete="off" disabled={fwd.busy !== null} value={vmTerminal} onChange={(event) => setVmTerminal(event.target.value)} />
            </label>
          </div>
          <div className="vm-identity__actions">
            <button
              type="button"
              className="button button--secondary"
              disabled={fwd.busy !== null}
              onClick={() => {
                void fwd.saveVmIdentity({
                  cliVersion,
                  originator: vmOriginator,
                  osType,
                  osVersion,
                  arch: vmArch,
                  terminal: vmTerminal,
                }).then((next) => {
                  if (next) applyVmDraft(next.vmIdentity);
                });
              }}
            >
              保存身份
            </button>
            <button type="button" className="button button--ghost" disabled={fwd.busy !== null} onClick={() => void fwd.detectVmVersion().then((next) => { if (next) applyVmDraft(next.vmIdentity); })}>检测本机 CLI</button>
            <button
              type="button"
              className="button button--ghost"
              disabled={fwd.busy !== null}
              onClick={() => {
                if (!window.confirm("重新生成 Installation ID 后，上游会把 Kit 看成一台新设备。确定继续？")) return;
                void fwd.regenerateVmInstallation();
              }}
            >
              换一台新机器
            </button>
          </div>
        </section>

        <footer className="page-footer">
          <span><Shield size={13} /> 本地运行 · 配置尽在掌握</span>
          <span>CODEX STATE KIT</span>
        </footer>
      </div>
    </AppShell>
  );
}
