import { t, formatCompactTokens, getLocale } from "@/lib/i18n";
import { useLocale } from "@/hooks/useLocale";
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
import { SHOW_SUSPECTED_DOWNGRADE_UI } from "@/lib/uiFlags";
import type { DevicePlatform, SavedAccount, Status } from "@/types";

function chipLabel(status: Status) {
  if (status.attached) return t("已接入");
  return status.proxyOk ? t("代理已开") : t("代理未开");
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

const PLATFORMS: { id: DevicePlatform; label: string }[] = [
  { id: "mac", label: "macOS" },
  { id: "windows", label: "Windows" },
  { id: "linux", label: "Linux" },
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
  useLocale();
  const fwd = useCodexStateKit();
  const [codexHome, setCodexHome] = useState("");
  const [outboundProxy, setOutboundProxy] = useState("");
  const [mihomoSubscription, setMihomoSubscription] = useState("");
  const [mihomoNode, setMihomoNode] = useState("");
  const [forcedModel, setForcedModel] = useState("");
  const [lanApiKey, setLanApiKey] = useState<string | null>(null);
  const [lanApiKeyCopyState, setLanApiKeyCopyState] = useState<"idle" | "copied" | "error">("idle");
  const [addAccountOpen, setAddAccountOpen] = useState(false);
  /** The saved account the login dialog re-authorizes; null adds a new one. */
  const [reauthTarget, setReauthTarget] = useState<SavedAccount | null>(null);
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
  useNotice("proxy-error", proxyError, () => ({ kind: "error", title: t("本地代理异常"), message: proxyError }));
  const attachError = fwd.status?.attachError ?? null;
  useNotice("attach-error", attachError, () => ({ kind: "error", title: t("Codex 接入失败"), message: attachError }));
  const mihomoError = fwd.status?.outboundMode === "mihomo" ? fwd.status?.mihomo?.error ?? null : null;
  useNotice("mihomo-error", mihomoError, () => ({ kind: "error", title: t("订阅节点异常"), message: mihomoError }));
  const relayError = fwd.status?.outboundMode === "manual" ? fwd.status?.systemProxy?.lastError ?? null : null;
  useNotice("system-proxy-error", relayError, () => ({ kind: "warn", title: t("连接代理服务器失败"), message: relayError }));
  const lastDowngrade = fwd.status?.lastDowngrade ?? null;
  const visibleDowngrade = lastDowngrade && (SHOW_SUSPECTED_DOWNGRADE_UI || lastDowngrade.report.verdict !== "suspected")
    ? lastDowngrade
    : null;
  useNotice("downgrade", visibleDowngrade?.requestId ?? null, () => {
    const event = visibleDowngrade!;
    const report = event.report;
    const who = event.email || event.accountId;
    const time = new Date(event.at).toLocaleTimeString(getLocale(), { hour12: false });
    return {
      kind: report.verdict === "confirmed" ? "error" : "warn",
      title: report.verdict === "confirmed" ? t("检测到降智请求") : t("检测到疑似降智请求"),
      message: t("{0} · {1} · 请求 {2}：{3}", [time, who, report.requestedModel ?? t("未知模型"), downgradeLabel(report)])
        + (report.useCases.length || report.reasons.length
          ? `（${[...report.useCases, ...report.reasons].join(" / ")}）`
          : ""),
      actions: [{ label: t("查看使用记录"), primary: true, onClick: () => selectTab("records") }],
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

  const activeAccount = fwd.accounts.find((account) => account.active);
  const bindingNote = activeAccount ? (
    <p className="binding-note">
      <Link2 size={12} aria-hidden="true" />
      {t("以下设置绑定到账号")} <strong>{accountName(activeAccount)}</strong>{t("，切换账号时会自动换成该账号自己的设置。")} </p>
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

  if (!fwd.status) {
    return (
      <AppShell>
        <div className="boot-screen">
          {fwd.error ? (
            <div className="boot-screen__error" role="alert">
              <strong>{t("无法启动 Codex State Kit")}</strong>
              <p>{fwd.error}</p>
              <button className="button button--primary" type="button" onClick={() => void fwd.refresh()}>
                {t("重试")} </button>
            </div>
          ) : (
            <>
              <span className="spinner spinner--blue" />
              {t("正在启动…")} </>
          )}
        </div>
      </AppShell>
    );
  }

  const vmIdentity = fwd.status.vmIdentity;

  const loggedIn = Boolean(fwd.login?.loggedIn);
  const tabAlert: Partial<Record<TabId, string>> = {
    network: fwd.status.proxyError || fwd.status.mihomo?.error ? t("出站网络异常") : undefined,
    account: loggedIn ? undefined : t("尚未登录"),
  };
  const selectedNode = mihomoNode || fwd.status.mihomo?.selected || "";
  const selectedNodeDelay = fwd.latency.mihomo?.samples.find((item) => item.name === selectedNode);
  const mihomoGroups = fwd.status?.mihomo.groups ?? [];
  const kitGroups = mihomoGroups.filter((group) => group.name === "Kit");
  const shownMihomoGroups = kitGroups.length > 0
    ? kitGroups
    : mihomoGroups.filter((group) => group.groupType === "select");
  const proxyPort = fwd.status.proxyListen.split(":").pop() ?? fwd.status.proxyListen;
  const lanBaseUrl = fwd.status.lanAccessUrl ?? `http://<Kit主机IP>:${proxyPort}`;

  async function copyLanApiKey() {
    if (!lanApiKey) return;
    try {
      await navigator.clipboard.writeText(lanApiKey);
      setLanApiKeyCopyState("copied");
      window.setTimeout(() => setLanApiKeyCopyState("idle"), 1800);
    } catch {
      setLanApiKeyCopyState("error");
    }
  }

  async function rotateLanApiKey() {
    const result = await fwd.rotateLanApiKey();
    if (!result) return;
    setLanApiKey(result.apiKey);
    setLanApiKeyCopyState("idle");
  }

  return (
    <AppShell>
      <div className="dash-page">
        <div className="page-nav">
          <div className="page-tabs" role="tablist" aria-label={t("页面分区")} onKeyDown={onTabKeyDown}>
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
                {t(label)}
                {tabAlert[id] ? <i className="page-tabs__alert" title={tabAlert[id]} aria-label={tabAlert[id]} /> : null}
              </button>
            ))}
          </div>
          <div className="page-actions">
            {fwd.accounts.length > 1 ? (
              <Select
                variant="compact"
                className="account-switcher"
                ariaLabel={t("切换账号")}
                placeholder={t("未使用已保存账号")}
                icon={<Users size={13} />}
                disabled={fwd.busy !== null || Boolean(fwd.device)}
                value={fwd.accounts.find((account) => account.active)?.accountId ?? ""}
                options={fwd.accounts.map((account) => ({
                  value: account.accountId,
                  label: accountName(account),
                  hint: account.usable ? undefined : t("需重新授权"),
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
        <section className="account-traffic" aria-label={t("当前账号请求统计")}>
          <div className="account-traffic__heading">
            <Activity size={19} aria-hidden="true" />
            <div>
              <h2>{t("当前账号请求")}</h2>
              <p>{isTauri ? (loggedIn ? t("经本机转发的业务请求") : t("登录后显示账号请求统计")) : t("浏览器示例数据 · 非实际请求")}</p>
            </div>
          </div>
          <dl className="account-traffic__metrics">
            <div title={t("此账号已经发起、尚未结束的上游业务请求；包含等待响应和流式输出阶段。")}>
              <dt>{t("当前并发")}</dt>
              <dd>{loggedIn ? fwd.status.accountTraffic?.concurrentRequests ?? "—" : "—"}<span>{t("请求")}</span></dd>
            </div>
            <div title={t("滚动最近 60 秒内发起的业务请求次数，包括失败请求。")}>
              <dt>RPM <span>{t("最近 60 秒")}</span></dt>
              <dd>{loggedIn ? fwd.status.accountTraffic?.rpm ?? "—" : "—"}<span>{t("次 / 分钟")}</span></dd>
            </div>
            <div title={t("滚动最近 60 秒内已收到用量统计的输入和输出 token 数；上游尚未返回用量的请求不会计入。")}>
              <dt>TPM <span>{t("最近 60 秒")}</span></dt>
              <dd>{loggedIn ? formatCompactTokens(fwd.status.accountTraffic?.tpm) : "—"}<span>{t("tokens / 分钟")}</span></dd>
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
            <div className="section-heading"><span className="section-icon"><Network size={19} /></span><div><h2>{t("出站网络")}</h2><p>{t("业务请求、登录和价格同步都走这一条出站线路")}</p></div></div>
          </header>
          {bindingNote}
          <div className="proxy-mode" role="group" aria-label={t("出站代理模式")}>
            <button type="button" aria-pressed={fwd.status.outboundMode === "manual"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveSettings(codexHome, outboundProxy, "manual")}><Network size={14} />{t("手动代理")}</button>
            <button type="button" aria-pressed={fwd.status.outboundMode === "mihomo"} disabled={fwd.busy !== null} onMouseDown={(event) => event.preventDefault()} onClick={() => void fwd.saveMihomo(mihomoSubscription, mihomoNode)}><Waypoints size={14} />{t("订阅节点")}</button>
          </div>
          {fwd.status.outboundMode === "manual" ? <>
          <div className="field">
            <span id="outbound-proxy-label">{t("代理 URL")}</span>
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
          <p className="panel__hint">{t("支持 socks5 / socks5h / http，离开输入框后自动保存。可把出口写成")} {'{session}'}{t("，Kit 自动生成会话出口；同一条上游连接沿用同一个 session。")}</p>
          <div className="system-proxy">
            <label className="system-proxy__toggle">
              <input
                type="checkbox"
                checked={fwd.status.chainSystemProxy !== false}
                disabled={fwd.busy !== null}
                onChange={(event) => void fwd.setChainSystemProxy(event.target.checked)}
              />
              {t("经系统代理连接代理服务器")} </label>
            <span className={fwd.status.systemProxy?.detected ? "system-proxy__state system-proxy__state--on" : "system-proxy__state"}>
              {fwd.status.chainSystemProxy === false
                ? t("已关闭，直连代理服务器")
                : fwd.status.systemProxy?.detected
                  ? t("检测到系统代理 {0}", [fwd.status.systemProxy.detected])
                  : t("未检测到系统代理，直连代理服务器")}
            </span>
            <p>{t("适用于 Clash Verge 等只开了系统代理、没开 TUN 的情况：代理服务器需要翻墙才能连上时，Kit 会先经系统代理再连到它。开关 Clash 的系统代理后自动跟随，无需重启。")}</p>
          </div>
          </> : (
          <div className="mihomo-panel">
            <label className="field">
              <span>{t("订阅地址")}</span>
              <input
                type="text"
                spellCheck={false}
                autoComplete="off"
                disabled={fwd.busy !== null}
                value={mihomoSubscription}
                placeholder={t("https://example.com/sub 或本地文件、分享链接")}
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
                  <span>{t("当前节点")}</span>
                  <div className="field-row">
                  <Select
                    ariaLabel={t("当前节点")}
                    placeholder={t("连接后列出节点")}
                    disabled={fwd.busy !== null || (fwd.status.mihomo?.nodes.length ?? 0) === 0}
                    value={mihomoNode || fwd.status.mihomo?.selected || ""}
                    options={(fwd.status.mihomo?.nodes ?? []).map((node) => {
                      const sample = fwd.latency.mihomo?.samples.find((item) => item.name === node);
                      return {
                        value: node,
                        label: node,
                        hint: sample ? (sample.delayMs != null ? `${sample.delayMs} ms` : t("超时")) : undefined,
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
            <div className="field-row codex-latency-row">
              <span className="panel__hint">{t("Codex 链路检测")}</span>
              <LatencyProbe probing={fwd.probing === "mihomo_codex"} disabled={fwd.probing !== null || fwd.status.mihomo?.phase !== "connected"}
                sample={fwd.latency.mihomo_codex?.samples[0]} onProbe={() => void fwd.probeLatency("mihomo_codex")} />
            </div>
            <p className="panel__hint">{t("节点测速使用轻量 204 地址，包含连接与 HTTPS 握手；Codex 链路单独检测上游 HTTP 响应，不代表模型首字速度。")}</p>
            <p className="panel__hint">
              {fwd.status.mihomo?.phase === "connected"
                ? t("已连接{0}{1}", [fwd.status.mihomo.selected ? ` · ${fwd.status.mihomo.selected}` : "", fwd.status.mihomo.proxyUrl ? ` · ${fwd.status.mihomo.proxyUrl}` : ""])
                : t("内核随应用内置。业务请求、登录和价格同步都走这条订阅线路。")}
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
          onReauthorize={(account) => {
            setReauthTarget(account);
            setAddAccountOpen(true);
          }}
          onAdd={() => {
            setReauthTarget(null);
            setAddAccountOpen(true);
          }}
        />
        <section className="panel">
          <header>
            <div className="section-heading"><span className="section-icon"><Settings2 size={19} /></span><div><h2>{t("转发设置")}</h2><p>{t("本机 Codex 目录与上游模型")}</p></div></div>
          </header>
          <label className="field">
            <span>{t("Codex 工作目录")}</span>
            <input spellCheck={false} disabled={fwd.busy !== null} value={codexHome} onChange={(event) => setCodexHome(event.target.value)}
              onBlur={() => void fwd.saveSettings(codexHome, outboundProxy)}
              onKeyDown={(event) => { if (event.key === "Enter") event.currentTarget.blur(); }} />
          </label>
          <label className="field">
            <span>{t("强制绑定模型")}</span>
            <input
              spellCheck={false}
              autoComplete="off"
              disabled={fwd.busy !== null}
              value={forcedModel}
              placeholder={t("例如 gpt-6-astra，留空按下游原模型转发")}
              onChange={(event) => setForcedModel(event.target.value)}
              onBlur={() => void fwd.saveForcedModel(forcedModel)}
              onKeyDown={(event) => {
                if (event.key === "Enter") event.currentTarget.blur();
              }}
            />
          </label>
          <p className="panel__hint">{t("填写后，下游无论请求什么模型 ID，都会改成这个值再转发给上游。")}</p>
          <section className="lan-access" aria-label={t("局域网访问")}>
            <div className="lan-access__heading">
              <div>
                <h3>{t("局域网访问")}</h3>
                <p>{t("让其他机器通过同一个端口调用 Kit")}</p>
              </div>
              <label className="lan-access__toggle">
                <input
                  type="checkbox"
                  checked={fwd.status.lanAccessEnabled === true}
                  disabled={fwd.busy !== null}
                  onChange={(event) => void fwd.setLanAccess(event.target.checked)}
                />
                <span>{fwd.status.lanAccessEnabled === true ? t("已开启") : t("已关闭")}</span>
              </label>
            </div>
            <p className="panel__hint">{t("开启后 Kit 会监听局域网地址；本机 Codex 仍使用当前端口和自动注入配置，远端客户端使用这里生成的 API Key。")}</p>
            <div className="lan-access__endpoint">
              <span>{t("远端 Base URL")}</span>
              <code>{lanBaseUrl}</code>
            </div>
            <div className="lan-access__key">
              <div className="lan-access__key-value">
                <span>{t("API Key")}</span>
                <code>{lanApiKey ?? fwd.status.lanApiKeyMasked ?? (fwd.status.lanApiKeyConfigured ? t("已配置（仅显示掩码）") : t("尚未生成"))}</code>
              </div>
              <div className="lan-access__actions">
                {lanApiKey ? (
                  <button type="button" className="button button--secondary" onClick={() => void copyLanApiKey()}>
                    {lanApiKeyCopyState === "copied" ? t("已复制") : t("复制 Key")}
                  </button>
                ) : null}
                <button type="button" className="button button--ghost" disabled={fwd.busy !== null} onClick={() => void rotateLanApiKey()}>
                  {fwd.status.lanApiKeyConfigured ? t("重新生成") : t("生成 API Key")}
                </button>
              </div>
            </div>
            {lanApiKeyCopyState === "error" ? <p className="lan-access__error">{t("复制失败，请手动复制上面的 Key")}</p> : null}
            <p className="lan-access__note">{t("Key 只在生成后显示一次，请保存到远端客户端的 API Key 配置中。重新生成会立即使旧 Key 失效。")}</p>
          </section>
        </section>
        </div>

        <section className="panel vm-panel tab-panel" role="tabpanel" id="tabpanel-device" aria-labelledby="tab-device" hidden={tab !== "device"}>
          <header>
            <div className="section-heading">
              <span className="section-icon"><Monitor size={19} /></span>
              <div>
                <h2>{t("虚拟设备")}</h2>
                <p>{vmIdentity?.enabled ? vmIdentity.userAgent : t("设备和环境信息原样透传")}</p>
              </div>
            </div>
            <span className="vm-identity__id">Installation {vmIdentity?.installationId ?? "—"}</span>
          </header>
          {bindingNote}
          {vmIdentity && <div className="vm-mode">
            <label className="vm-mode__toggle">
              <input
                type="checkbox"
                checked={vmIdentity.enabled}
                disabled={fwd.busy !== null}
                onChange={event => void fwd.saveVmIdentity({ platform: vmIdentity.platform, enabled: event.target.checked })}
              />
              <span>
                <strong>{t("启用虚拟设备模拟")}</strong>
                <small>{vmIdentity.enabled ? t("改写设备指纹和模型环境") : t("关闭后纯透传客户端的设备和环境")}</small>
              </span>
            </label>
          </div>}
          <div className="field">
            <span>{t("系统")}</span>
            <div className="proxy-mode vm-platforms" role="group" aria-label={t("系统")}>
              {PLATFORMS.map(({ id, label }) => (
                <button
                  key={id}
                  type="button"
                  aria-pressed={vmIdentity?.platform === id}
                  disabled={fwd.busy !== null || !vmIdentity?.enabled}
                  onMouseDown={(event) => event.preventDefault()}
                  onClick={async () => {
                    if (!vmIdentity || vmIdentity.platform === id) return;
                    const ok = await confirm({
                      title: t("把虚拟设备换成 {0}？", [label]),
                      message: t("系统版本、架构和终端会一起换成该系统的参数。上游会看到这台设备的系统变了，没有必要时不要来回切换。"),
                      confirmText: t("切换系统"),
                    });
                    if (ok) void fwd.saveVmIdentity({ platform: id });
                  }}
                >
                  {label}
                </button>
              ))}
            </div>
          </div>
          <dl className="vm-identity__grid">
            {([
              [t("CLI 版本"), vmIdentity?.cliVersion],
              ["Originator", vmIdentity?.originator],
              [t("系统版本"), vmIdentity ? `${vmIdentity.osType} ${vmIdentity.osVersion}` : undefined],
              [t("架构"), vmIdentity?.arch],
              [t("终端"), vmIdentity ? `${vmIdentity.terminal}${vmIdentity.terminalVersion ? `/${vmIdentity.terminalVersion}` : ""}` : undefined],
              [t("终端复用器"), vmIdentity?.terminalMultiplexer || t("未检测到")],
              [t("地区"), vmIdentity?.environment?.region || t("未识别")],
              [t("模型时区"), vmIdentity?.environment?.timezone || t("沿用客户端")],
              [t("语言区域"), vmIdentity?.environment?.locale || t("沿用客户端")],
            ] as const).map(([term, value]) => (
              <div key={term} className="vm-identity__item">
                <dt>{term}</dt>
                <dd>{value ?? "—"}</dd>
              </div>
            ))}
          </dl>
          {vmIdentity && <div className="vm-environment">
            <div className="vm-environment__top">
              <div className="vm-environment__title"><span className="vm-environment__dot" aria-hidden="true" /><span>{t("模型环境")}</span><small>{vmIdentity.enabled ? (vmIdentity.environment?.autoRegion ?? true ? t("随代理出口自动同步") : t("手动设置")) : t("当前透传")}</small></div>
              <label className="vm-environment__toggle">
              <input type="checkbox" checked={vmIdentity.environment?.autoRegion ?? true} disabled={fwd.busy !== null || !vmIdentity.enabled}
                onChange={event => void fwd.saveVmIdentity({ platform: vmIdentity.platform, environment: {
                  timezone: "", locale: "", region: "", ...vmIdentity.environment, autoRegion: event.target.checked,
                } })} />  {t("自动探测")} </label>
            </div>
            <p className="vm-environment__hint">{vmIdentity.enabled ? t("根据代理出口 IP 同步时区与语言，日期随时区计算。") : t("已关闭环境模拟，请求中的环境信息会原样发送。")}</p>
            {!(vmIdentity.environment?.autoRegion ?? true) && ([
              ["timezone", t("IANA 时区"), "Asia/Tokyo"],
              ["locale", t("语言区域"), "zh-CN"],
            ] as const).map(([key, label, placeholder]) => <label key={key} className="field">
              <span>{label}</span>
              <input key={`${vmIdentity.installationId}-${key}-${vmIdentity.environment?.[key]}`} defaultValue={vmIdentity.environment?.[key] ?? ""}
                placeholder={placeholder} disabled={fwd.busy !== null || !vmIdentity.enabled}
                onBlur={event => {
                  const value = event.target.value.trim();
                  if (value === (vmIdentity.environment?.[key] ?? "")) return;
                  void fwd.saveVmIdentity({ platform: vmIdentity.platform, environment: {
                    autoRegion: false, timezone: "", locale: "", region: "", ...vmIdentity.environment, [key]: value,
                  } });
                }} onKeyDown={event => { if (event.key === "Enter") event.currentTarget.blur(); }} />
            </label>)}
          </div>}
          <p className="panel__hint">{vmIdentity?.enabled ? t("系统版本、架构和终端跟随所选系统，与官方 CLI 在该系统上上报的一致；CLI 版本和终端信息跟随本机环境。") : t("当前为纯透传模式，下面保存的虚拟设备参数不会改写请求。")}</p>
          <div className="vm-identity__actions">
            <button type="button" className="button button--ghost" disabled={fwd.busy !== null || !vmIdentity?.enabled} onClick={() => void fwd.detectVmVersion()}>{t("检测本机 CLI")}</button>
            <button
              type="button"
              className="button button--ghost"
              disabled={fwd.busy !== null || !vmIdentity?.enabled}
              onClick={async () => {
                const ok = await confirm({
                  title: t("换一台新机器？"),
                  message: t("重新生成 Installation ID 后，上游会把当前账号看成一台新设备。"),
                  confirmText: t("换新机器"),
                  danger: true,
                });
                if (ok) void fwd.regenerateVmInstallation();
              }}
            >
              {t("换一台新机器")} </button>
          </div>
        </section>

        <AddAccountDialog
          open={addAccountOpen}
          target={reauthTarget}
          onClose={() => setAddAccountOpen(false)}
          fwd={fwd}
          codexHome={codexHome}
        />

        <footer className="page-footer">
          <span><Shield size={13} />  {t("本地运行 · 配置尽在掌握")}</span>
          <span>CODEX STATE KIT</span>
        </footer>
      </div>
    </AppShell>
  );
}
