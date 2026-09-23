import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import Shield from "lucide-react/dist/esm/icons/shield.js";
import Activity from "lucide-react/dist/esm/icons/activity.js";
import Monitor from "lucide-react/dist/esm/icons/monitor.js";
import Network from "lucide-react/dist/esm/icons/network.js";
import Terminal from "lucide-react/dist/esm/icons/terminal.js";
import Waypoints from "lucide-react/dist/esm/icons/waypoints.js";
import LayoutDashboard from "lucide-react/dist/esm/icons/layout-dashboard.js";
import Settings2 from "lucide-react/dist/esm/icons/settings-2.js";
import Users from "lucide-react/dist/esm/icons/users.js";
import Link2 from "lucide-react/dist/esm/icons/link-2.js";
import ScrollText from "lucide-react/dist/esm/icons/scroll-text.js";
import BadgeDollarSign from "lucide-react/dist/esm/icons/badge-dollar-sign.js";
import { AppShell } from "@/components/AppShell";
import { BillingPanel } from "@/components/BillingPanel";
import { REFRESH_OPTIONS } from "@/components/RefreshControl";
import { MihomoGroupPanel } from "@/components/MihomoGroupPanel";
import { downgradeLabel, UsageRecordsPanel } from "@/components/UsageRecordsPanel";
import { PricingPanel } from "@/components/PricingPanel";
import { AccountsPanel, accountName } from "@/components/AccountsPanel";
import { AddAccountDialog } from "@/components/AddAccountDialog";
import { Select } from "@/components/Select";
import { LatencyProbe } from "@/components/LatencyProbe";
import { useNotice, useNotify } from "@/components/Notifier";
import { useCodexStateKit } from "@/hooks/useCodexStateKit";
import { isTauri } from "@/lib/api";
import type { Status, VmIdentityView } from "@/types";

function chipLabel(status: Status) {
  if (status.attached) return "已接入";
  return status.proxyOk ? "代理已开" : "代理未开";
}

function chipClass(status: Status) {
  if (status.attached) return "runtime-chip runtime-chip--accent";
  return status.proxyOk ? "runtime-chip" : "runtime-chip runtime-chip--down";
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
const REFRESH_STORAGE_KEY = "codex-state-kit.refresh-ms";

/** Auto-refresh interval for the overview and usage records; 1 s by default. */
function savedRefreshMs(): number {
  try {
    const saved = window.localStorage.getItem(REFRESH_STORAGE_KEY);
    if (saved !== null && REFRESH_OPTIONS.some((option) => option.value === saved)) return Number(saved);
  } catch {
    // storage unavailable
  }
  return 1000;
}

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
  const [addAccountOpen, setAddAccountOpen] = useState(false);
  const [tab, setTab] = useState<TabId>(initialTab);
  const [refreshMs, setRefreshMs] = useState(savedRefreshMs);
  const changeRefreshMs = (intervalMs: number) => {
    setRefreshMs(intervalMs);
    try {
      window.localStorage.setItem(REFRESH_STORAGE_KEY, String(intervalMs));
    } catch {
      // storage unavailable
    }
  };
  const tabRefs = useRef<Partial<Record<TabId, HTMLButtonElement | null>>>({});
  const hydrated = useRef(false);
  const { notify, confirm } = useNotify();

  // Action results from the backend hook become themed notices.
  useEffect(() => {
    if (fwd.banner) notify({ kind: fwd.banner.kind, message: fwd.banner.text });
  }, [fwd.banner, notify]);

  const proxyError = fwd.status?.proxyError ?? null;
  useNotice("proxy-error", proxyError, () => ({ kind: "error", title: "本地代理异常", message: proxyError }));
  const attachError = fwd.status?.attachError ?? null;
  useNotice("attach-error", attachError, () => ({ kind: "error", title: "Codex 接入失败", message: attachError }));
  const mihomoError = fwd.status?.outboundMode === "mihomo" ? fwd.status?.mihomo?.error ?? null : null;
  useNotice("mihomo-error", mihomoError, () => ({ kind: "error", title: "订阅节点异常", message: mihomoError }));
  const relayError = fwd.status?.outboundMode === "manual" ? fwd.status?.systemProxy?.lastError ?? null : null;
  useNotice("system-proxy-error", relayError, () => ({ kind: "warn", title: "连接代理服务器失败", message: relayError }));
  const lastDowngrade = fwd.status?.lastDowngrade ?? null;
  useNotice("downgrade", lastDowngrade?.requestId ?? null, () => {
    const event = lastDowngrade!;
    const report = event.report;
    const who = event.email || event.accountId;
    const time = new Date(event.at).toLocaleTimeString("zh-CN", { hour12: false });
    return {
      kind: report.verdict === "confirmed" ? "error" : "warn",
      title: report.verdict === "confirmed" ? "检测到降智请求" : "检测到疑似降智请求",
      message: `${time} · ${who} · 请求 ${report.requestedModel ?? "未知模型"}：${downgradeLabel(report)}`
        + (report.useCases.length || report.reasons.length
          ? `（${[...report.useCases, ...report.reasons].join(" / ")}）`
          : ""),
      actions: [{ label: "查看使用记录", primary: true, onClick: () => selectTab("records") }],
    };
  });

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

  // Switching accounts swaps the bound outbound line and virtual device on
  // the backend; follow those values. Unsaved typing is untouched because
  // these only change after a save or a switch.
  useEffect(() => {
    if (!fwd.status || !hydrated.current) return;
    setOutboundProxy(fwd.status.outboundProxy ?? "");
    setMihomoSubscription(fwd.status.mihomoSubscription ?? "");
    setMihomoNode(fwd.status.mihomoNode ?? "");
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fwd.status?.outboundProxy, fwd.status?.mihomoSubscription, fwd.status?.mihomoNode]);

  useEffect(() => {
    const identity = fwd.status?.vmIdentity;
    if (!identity || !hydrated.current) return;
    applyVmDraft(identity);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fwd.status?.vmIdentity?.installationId, fwd.status?.vmIdentity?.userAgent]);

  const activeAccount = fwd.accounts.find((account) => account.active);
  const bindingNote = activeAccount ? (
    <p className="binding-note">
      <Link2 size={12} aria-hidden="true" />
      以下设置绑定到账号 <strong>{accountName(activeAccount)}</strong>，切换账号时会自动换成该账号自己的设置。
    </p>
  ) : null;

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
            {fwd.accounts.length > 1 ? (
              <Select
                variant="compact"
                className="account-switcher"
                ariaLabel="切换账号"
                placeholder="未使用已保存账号"
                icon={<Users size={13} />}
                disabled={fwd.busy !== null || Boolean(fwd.device)}
                value={fwd.accounts.find((account) => account.active)?.accountId ?? ""}
                options={fwd.accounts.map((account) => ({
                  value: account.accountId,
                  label: accountName(account),
                  hint: account.label && account.email ? account.email : undefined,
                  disabled: !account.usable,
                }))}
                onChange={(accountId) => void fwd.switchToAccount(accountId)}
              />
            ) : null}
            <span className={chipClass(fwd.status)}>
              <i />
              {chipLabel(fwd.status)}
            </span>
          </div>
        </div>



        <div className="tab-panel" role="tabpanel" id="tabpanel-overview" aria-labelledby="tab-overview" hidden={tab !== "overview"}>
        <section className="account-traffic" aria-label="当前账号请求统计">
          <div className="account-traffic__heading">
            <Activity size={19} aria-hidden="true" />
            <div>
              <h2>当前账号请求</h2>
              <p>{isTauri ? (loggedIn ? "经本机转发的业务请求" : "登录后显示账号请求统计") : "浏览器示例数据 · 非实际请求"}</p>
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
          savedAccounts={fwd.accounts}
          active={tab === "overview"}
          refreshMs={refreshMs}
          onRefreshMsChange={changeRefreshMs}
        />
        </div>


        <section className="panel tab-panel tab-panel--flush" role="tabpanel" id="tabpanel-records" aria-labelledby="tab-records" hidden={tab !== "records"}>
          <UsageRecordsPanel active={tab === "records"} status={fwd.status} savedAccounts={fwd.accounts} refreshMs={refreshMs} onRefreshMsChange={changeRefreshMs} />
        </section>

        <section className="panel tab-panel tab-panel--flush" role="tabpanel" id="tabpanel-pricing" aria-labelledby="tab-pricing" hidden={tab !== "pricing"}>
          <PricingPanel active={tab === "pricing"} />
        </section>

        <section className="panel tab-panel" role="tabpanel" id="tabpanel-network" aria-labelledby="tab-network" hidden={tab !== "network"}>
          <header>
            <div className="section-heading"><span className="section-icon"><Network size={19} /></span><div><h2>出站网络</h2><p>业务请求、登录和价格同步都走这一条出站线路</p></div></div>
          </header>
          {bindingNote}
          <div className="proxy-mode" role="group" aria-label="出站代理模式">
            <button type="button" aria-pressed={fwd.status.outboundMode === "manual"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveSettings(codexHome, outboundProxy, "manual")}><Network size={14} />手动代理</button>
            <button type="button" aria-pressed={fwd.status.outboundMode === "mihomo"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveMihomo(mihomoSubscription, mihomoNode)}><Waypoints size={14} />订阅节点</button>
          </div>
          {fwd.status.outboundMode === "manual" ? <>
          <div className="field">
            <span id="outbound-proxy-label">代理 URL</span>
            <div className="field-row">
              <input
                type="text"
                aria-labelledby="outbound-proxy-label"
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
              <LatencyProbe
                probing={fwd.probing === "manual"}
                disabled={fwd.probing !== null || !outboundProxy.trim()}
                sample={fwd.latency.manual?.samples[0]}
                onProbe={() => void fwd.probeLatency("manual", outboundProxy)}
              />
            </div>
          </div>
          <p className="panel__hint">支持 socks5 / socks5h / http，离开输入框后自动保存。可把出口写成 {'{session}'}，Kit 自动生成会话出口；同一条上游连接沿用同一个 session。</p>
          <div className="system-proxy">
            <label className="system-proxy__toggle">
              <input
                type="checkbox"
                checked={fwd.status.chainSystemProxy !== false}
                disabled={fwd.busy !== null}
                onChange={(event) => void fwd.setChainSystemProxy(event.target.checked)}
              />
              经系统代理连接代理服务器
            </label>
            <span className={fwd.status.systemProxy?.detected ? "system-proxy__state system-proxy__state--on" : "system-proxy__state"}>
              {fwd.status.chainSystemProxy === false
                ? "已关闭，直连代理服务器"
                : fwd.status.systemProxy?.detected
                  ? `检测到系统代理 ${fwd.status.systemProxy.detected}`
                  : "未检测到系统代理，直连代理服务器"}
            </span>
            <p>适用于 Clash Verge 等只开了系统代理、没开 TUN 的情况：代理服务器需要翻墙才能连上时，Kit 会先经系统代理再连到它。开关 Clash 的系统代理后自动跟随，无需重启。</p>
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
                <div className="field">
                  <span>当前节点</span>
                  <div className="field-row">
                  <Select
                    ariaLabel="当前节点"
                    placeholder="连接后列出节点"
                    disabled={fwd.busy !== null || (fwd.status.mihomo?.nodes.length ?? 0) === 0}
                    value={mihomoNode || fwd.status.mihomo?.selected || ""}
                    options={(fwd.status.mihomo?.nodes ?? []).map((node) => {
                      const sample = fwd.latency.mihomo?.samples.find((item) => item.name === node);
                      return {
                        value: node,
                        label: node,
                        hint: sample ? (sample.delayMs != null ? `${sample.delayMs} ms` : "超时") : undefined,
                      };
                    })}
                    onChange={(node) => {
                      setMihomoNode(node);
                      void fwd.saveMihomo(mihomoSubscription, node);
                    }}
                  />
                  <LatencyProbe
                    probing={fwd.probing === "mihomo"}
                    disabled={fwd.probing !== null || (isTauri && fwd.status.mihomo?.phase !== "connected")}
                    sample={selectedNodeDelay}
                    onProbe={() => void fwd.probeLatency("mihomo")}
                  />
                  </div>
                </div>
              </>
            )}
            <p className="panel__hint">
              {fwd.status.mihomo?.phase === "connected"
                ? `已连接${fwd.status.mihomo.selected ? ` · ${fwd.status.mihomo.selected}` : ""}${fwd.status.mihomo.proxyUrl ? ` · ${fwd.status.mihomo.proxyUrl}` : ""}`
                : "内核随应用内置。业务请求、登录和价格同步都走这条订阅线路。"}
            </p>
          </div>
          )}
        </section>

        <div className="tab-panel" role="tabpanel" id="tabpanel-account" aria-labelledby="tab-account" hidden={tab !== "account"}>
        <AccountsPanel
          accounts={fwd.accounts}
          busy={fwd.busy !== null || Boolean(fwd.device)}
          onSwitch={(accountId) => void fwd.switchToAccount(accountId)}
          onRemove={(accountId) => void fwd.removeSavedAccount(accountId)}
          onRename={(accountId, label) => void fwd.renameSavedAccount(accountId, label)}
          onAdd={() => setAddAccountOpen(true)}
        />
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
          <p className="panel__hint">填写后，下游无论请求什么模型 ID，都会改成这个值再转发给上游。</p>
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
          {bindingNote}
          <div className="vm-identity__grid">
            <label className="field">
              <span>CLI 版本</span>
              <input type="text" spellCheck={false} autoComplete="off" disabled={fwd.busy !== null} value={cliVersion} onChange={(event) => setCliVersion(event.target.value)} />
            </label>
            <label className="field">
              <span>Originator</span>
              <input type="text" spellCheck={false} autoComplete="off" disabled={fwd.busy !== null} value={vmOriginator} onChange={(event) => setVmOriginator(event.target.value)} />
            </label>
            <div className="field">
              <span>系统</span>
              <Select
                ariaLabel="系统"
                disabled={fwd.busy !== null}
                value={osType}
                options={["Mac OS", "Linux", "Windows"].map((value) => ({ value, label: value }))}
                onChange={setOsType}
              />
            </div>
            <label className="field">
              <span>系统版本</span>
              <input type="text" spellCheck={false} autoComplete="off" disabled={fwd.busy !== null} value={osVersion} onChange={(event) => setOsVersion(event.target.value)} />
            </label>
            <div className="field">
              <span>架构</span>
              <Select
                ariaLabel="架构"
                disabled={fwd.busy !== null}
                value={vmArch}
                options={["arm64", "x86_64"].map((value) => ({ value, label: value }))}
                onChange={setVmArch}
              />
            </div>
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
              onClick={async () => {
                const ok = await confirm({
                  title: "换一台新机器？",
                  message: "重新生成 Installation ID 后，上游会把当前账号看成一台新设备。",
                  confirmText: "换新机器",
                  danger: true,
                });
                if (ok) void fwd.regenerateVmInstallation();
              }}
            >
              换一台新机器
            </button>
          </div>
        </section>

        <AddAccountDialog
          open={addAccountOpen}
          onClose={() => setAddAccountOpen(false)}
          fwd={fwd}
          codexHome={codexHome}
        />

        <footer className="page-footer">
          <span><Shield size={13} /> 本地运行 · 配置尽在掌握</span>
          <span>CODEX STATE KIT</span>
        </footer>
      </div>
    </AppShell>
  );
}
