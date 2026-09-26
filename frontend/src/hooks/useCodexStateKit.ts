import { t } from "@/lib/i18n";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  cancelChatgptLogin,
  getLoginStatus,
  getStatus,
  importChatgptAccessToken,
  importChatgptRefreshToken,
  pollChatgptLogin,
  setConfig,
  setLanAccess as setLanAccessApi,
  openUrl,
  startChatgptLogin,
  probeOutboundLatency,
  mihomoSelect,
  mihomoGroupDelay,
  updateVmIdentity,
  regenerateVmInstallationId,
  regenerateLanApiKey,
  detectVmCliVersion,
  listAccounts,
  onAccountsChanged,
  removeAccount,
  switchAccount,
} from "@/lib/api";
import type { SavedAccount, Banner, LoginMethod, LoginStart, LoginStatus, Status, OutboundMode, SettingsPatch, ProbeKind, LatencyReport, VmProfile } from "@/types";

function patchFrom(status: Status, overrides: Partial<SettingsPatch> = {}): SettingsPatch {
  return {
    proxyListen: status.proxyListen,
    lanAccessEnabled: status.lanAccessEnabled,
    upstream: status.upstream,
    codexHome: status.codexHome,
    outboundProxy: status.outboundProxy,
    outboundMode: status.outboundMode,
    mihomoSubscription: status.mihomoSubscription ?? "",
    mihomoNode: status.mihomoNode ?? "",
    forcedModel: status.forcedModel ?? "",
    chainSystemProxy: status.chainSystemProxy !== false,
    ...overrides,
  };
}

function errorMessage(cause: unknown): string {
  if (typeof cause === "string") return t(cause);
  if (cause instanceof Error) return t(cause.message);
  if (cause && typeof cause === "object" && "message" in cause) {
    return t(String((cause as { message: unknown }).message));
  }
  return t(String(cause));
}

/** Success notice; warns when a re-authorization signed in to another account. */
function loginBanner(login: LoginStatus | null, expected: string | undefined, text: string): Banner {
  if (expected && login?.accountId && login.accountId !== expected) {
    return { kind: "warn", text: t("登录的是 {0}，不是要重新授权的账号，已作为另一个账号保存", [login.email ?? login.accountId]) };
  }
  if (expected) return { kind: "ok", text: t("已重新授权 {0}", [login?.email ?? expected]) };
  return { kind: "ok", text: t(text) };
}

export function useCodexStateKit() {
  const [status, setStatus] = useState<Status | null>(null);
  const [login, setLogin] = useState<LoginStatus | null>(null);
  const [accounts, setAccounts] = useState<SavedAccount[]>([]);
  const [device, setDevice] = useState<LoginStart | null>(null);
  const [banner, setBanner] = useState<Banner | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState<"login" | "save" | "refresh" | null>(null);
  const [probing, setProbing] = useState<ProbeKind | null>(null);
  const [probingGroup, setProbingGroup] = useState<string | null>(null);
  const [latency, setLatency] = useState<Partial<Record<ProbeKind, LatencyReport>>>({});
  const request = useRef<Promise<void> | null>(null);
  const pollTimer = useRef<number | null>(null);
  const loginRequest = useRef(0);
  const loginGeneration = useRef(0);

  const loadStatus = useCallback(async (silent: boolean) => {
    if (silent && request.current) return;
    while (request.current) {
      await request.current;
    }
    if (!silent) setError(null);
    const next = getStatus()
      .then((value) => {
        setStatus(value);
      })
      .catch((cause) => {
        const message = errorMessage(cause);
        if (silent) return;
        setError(message);
      });
    request.current = next;
    try {
      await next;
    } finally {
      if (request.current === next) request.current = null;
    }
  }, []);

  const refresh = useCallback(() => loadStatus(false), [loadStatus]);

  const loadLogin = useCallback(async (home?: string) => {
    const revision = ++loginRequest.current;
    try {
      const next = await getLoginStatus(home);
      if (revision === loginRequest.current) setLogin(next);
    } catch (cause) {
      if (revision === loginRequest.current) setBanner({ kind: "error", text: errorMessage(cause) });
    }
  }, []);

  useEffect(() => {
    void refresh();
    const timer = window.setInterval(() => void loadStatus(true), 1000);
    return () => window.clearInterval(timer);
  }, [loadStatus, refresh]);

  useEffect(() => {
    if (!status) return;
    void loadLogin(status.codexHome);
  }, [loadLogin, status?.codexHome]);

  const codexHome = status?.codexHome;

  const loadAccounts = useCallback(async () => {
    try {
      setAccounts(await listAccounts(codexHome));
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    }
  }, [codexHome]);

  // A new login is saved into the account list by the backend; reload it
  // whenever the logged-in account changes.
  useEffect(() => {
    if (!codexHome) return;
    void loadAccounts();
  }, [loadAccounts, codexHome, login?.accountId]);

  // The tray menu can switch accounts while the window is open.
  useEffect(() => {
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void onAccountsChanged((payload) => {
      setBanner({ kind: payload.ok ? "ok" : "error", text: payload.message });
      void loadAccounts();
      if (codexHome) void loadLogin(codexHome);
      void loadStatus(true);
    }).then((stop) => {
      if (disposed) stop();
      else unlisten = stop;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, [codexHome, loadAccounts, loadLogin, loadStatus]);

  const switchToAccount = useCallback(async (accountId: string) => {
    setBusy("login");
    try {
      const next = await switchAccount(accountId, codexHome);
      setLogin(next);
      setBanner({ kind: "ok", text: t("已切换到 {0}，后续请求立即使用该账号", [next.email ?? accountId]) });
      await loadAccounts();
      void loadStatus(true);
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, [codexHome, loadAccounts, loadStatus]);

  const removeSavedAccount = useCallback(async (accountId: string) => {
    try {
      await removeAccount(accountId, codexHome);
      await loadAccounts();
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    }
  }, [codexHome, loadAccounts]);

  const stopPolling = useCallback(() => {
    loginGeneration.current += 1;
    if (pollTimer.current !== null) {
      window.clearTimeout(pollTimer.current);
      pollTimer.current = null;
    }
  }, []);

  useEffect(() => () => stopPolling(), [stopPolling]);

  const persistSettings = useCallback(async (home: string, outboundProxy: string, current: Status, outboundMode = current.outboundMode) => {
    if (home === current.codexHome && outboundProxy === current.outboundProxy && outboundMode === current.outboundMode) return current;
    return setConfig(patchFrom(current, { codexHome: home, outboundProxy, outboundMode }));
  }, []);

  const saveMihomo = useCallback(async (subscription: string, node: string) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig(patchFrom(latest, {
        outboundMode: "mihomo",
        mihomoSubscription: subscription.trim(),
        mihomoNode: node.trim(),
      }));
      setStatus(next);
      setBanner(null);
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const saveSettings = useCallback(async (home: string, outboundProxy: string, outboundMode?: OutboundMode) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      if (home.trim() !== latest.codexHome) {
        stopPolling();
        setDevice(null);
        await cancelChatgptLogin();
      }
      const next = await persistSettings(home.trim(), outboundProxy, latest, outboundMode);
      if (next.codexHome !== latest.codexHome) {
        await loadLogin(next.codexHome);
      }
      setStatus(next);
      setBanner(null);
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, [persistSettings, loadLogin, stopPolling]);

  const probeLatency = useCallback(async (kind: ProbeKind, proxy?: string) => {
    setProbing(kind);
    try {
      const report = await probeOutboundLatency(kind, proxy);
      setLatency((current) => ({ ...current, [kind]: report }));
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setProbing(null);
    }
  }, []);

  const saveForcedModel = useCallback(async (forcedModel: string) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig(patchFrom(latest, { forcedModel: forcedModel.trim() }));
      setStatus(next);
      setBanner({ kind: "ok", text: next.forcedModel
        ? t("已强制绑定模型 {0}，下游请求都会改成这个 ID 再转发。", [next.forcedModel])
        : t("已关闭强制绑定模型，按下游请求的模型 ID 转发。") });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const selectMihomoNode = useCallback(async (group: string, node: string) => {
    setBusy("save");
    try {
      await mihomoSelect(group, node);
      setStatus(await getStatus());
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const probeMihomoGroup = useCallback(async (group: string, node?: string) => {
    setProbingGroup(group);
    try {
      await mihomoGroupDelay(group, node);
      setStatus(await getStatus());
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setProbingGroup(null);
    }
  }, []);

  const probeAllMihomo = useCallback(async () => {
    const groups = (status?.mihomo.groups ?? []).filter((group) => group.groupType === "select" || group.groupType === "url-test");
    setProbingGroup("*");
    try {
      await Promise.all(groups.map((group) => mihomoGroupDelay(group.name)));
      setStatus(await getStatus());
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setProbingGroup(null);
    }
  }, [status]);

  /** `accountId`: the saved account being re-authorized, if any. */
  const startLogin = useCallback(async (home: string, method: LoginMethod, accountId?: string) => {
    setBusy("login");
    stopPolling();
    const generation = loginGeneration.current;
    try {
      const started = await startChatgptLogin(home, method, accountId);
      if (generation !== loginGeneration.current) return;
      setBanner(null);
      setDevice(started);
      try {
        await openUrl(started.verificationUri);
      } catch (cause) {
        if (generation === loginGeneration.current) setBanner({ kind: "error", text: errorMessage(cause) });
      }
      if (generation !== loginGeneration.current) return;
      const tick = async () => {
        try {
          const poll = await pollChatgptLogin();
          if (generation !== loginGeneration.current) return;
          if (poll.status === "pending") {
            pollTimer.current = window.setTimeout(() => void tick(), Math.max(1, started.interval) * 1000);
            return;
          }
          const loggedIn = poll.status === "ok" ? poll.login ?? await getLoginStatus(home) : null;
          if (generation !== loginGeneration.current) return;
          stopPolling();
          setDevice(null);
          if (poll.status === "ok") {
            setLogin(loggedIn);
            setBanner(loginBanner(loggedIn, accountId, poll.message ? t(poll.message) : t("已登录 ChatGPT")));
          } else {
            setBanner({ kind: "error", text: poll.message ? t(poll.message) : t("登录失败") });
          }
        } catch (cause) {
          if (generation !== loginGeneration.current) return;
          stopPolling();
          setDevice(null);
          setBanner({ kind: "error", text: errorMessage(cause) });
        }
      };
      pollTimer.current = window.setTimeout(() => void tick(), 1000);
    } catch (cause) {
      if (generation !== loginGeneration.current) return;
      setBanner({ kind: "error", text: errorMessage(cause) });
      setDevice(null);
    } finally {
      setBusy(null);
    }
  }, [stopPolling]);

  const importRefreshLogin = useCallback(async (home: string, refreshToken: string, accountId?: string) => {
    setBusy("login");
    stopPolling();
    setDevice(null);
    try {
      const loggedIn = await importChatgptRefreshToken(home.trim(), refreshToken.trim(), accountId);
      setLogin(loggedIn);
      setBanner(loginBanner(loggedIn, accountId, t("Refresh Token 已换取并同步到 Codex 账号")));
      return true;
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
      return false;
    } finally {
      setBusy(null);
    }
  }, [stopPolling]);

  const importAccessLogin = useCallback(async (home: string, accessToken: string, accountId?: string) => {
    setBusy("login");
    stopPolling();
    setDevice(null);
    try {
      const loggedIn = await importChatgptAccessToken(home.trim(), accessToken.trim());
      setLogin(loggedIn);
      setBanner(loginBanner(loggedIn, accountId, t("Access Token 已同步到 Codex 账号")));
      return true;
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
      return false;
    } finally {
      setBusy(null);
    }
  }, [stopPolling]);

  const cancelLogin = useCallback(async () => {
    stopPolling();
    setDevice(null);
    try {
      await cancelChatgptLogin();
      setBanner({ kind: "ok", text: t("已取消登录") });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    }
  }, [stopPolling]);

  const openLoginPage = useCallback(async () => {
    const url = device?.verificationUri;
    if (!url) return;
    try {
      await openUrl(url);
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    }
  }, [device]);

  const setChainSystemProxy = useCallback(async (enabled: boolean) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig(patchFrom(latest, { chainSystemProxy: enabled }));
      setStatus(next);
      setBanner({ kind: "ok", text: enabled ? t("手动代理将经系统代理连接（检测到时）") : t("手动代理改为直连") });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const setLanAccess = useCallback(async (enabled: boolean) => {
    setBusy("save");
    try {
      const next = await setLanAccessApi(enabled);
      setStatus(next);
      setBanner({ kind: "ok", text: enabled ? t("已允许局域网访问") : t("已关闭局域网访问") });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const rotateLanApiKey = useCallback(async () => {
    setBusy("save");
    try {
      const result = await regenerateLanApiKey();
      // The plaintext key is deliberately held only by the caller. Returning it
      // lets the UI copy/show it once while status polling retains only a mask.
      const next = await getStatus();
      setStatus(next);
      setBanner({ kind: "ok", text: t("API Key 已生成，请立即复制保存") });
      return result;
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
      return null;
    } finally {
      setBusy(null);
    }
  }, []);

  const saveVmIdentity = useCallback(async (profile: VmProfile) => {
    setBusy("save");
    try {
      const next = await updateVmIdentity(profile);
      setStatus(next);
      setBanner({
        kind: "ok",
        text: profile.enabled === false
          ? t("已关闭虚拟设备模拟，之后的请求将原样透传设备和环境")
          : t("虚拟设备身份已保存，之后的请求都使用这份指纹"),
      });
      return next;
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
      return null;
    } finally {
      setBusy(null);
    }
  }, []);

  const regenerateVmInstallation = useCallback(async () => {
    setBusy("save");
    try {
      const next = await regenerateVmInstallationId();
      setStatus(next);
      setBanner({ kind: "ok", text: t("已换成新的 Installation ID，上游会把 Kit 看成一台新设备") });
      return next;
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
      return null;
    } finally {
      setBusy(null);
    }
  }, []);

  const detectVmVersion = useCallback(async () => {
    setBusy("save");
    try {
      const next = await detectVmCliVersion();
      setStatus(next);
      setBanner({ kind: "ok", text: t("已对齐本机 Codex CLI {0}", [next.vmIdentity.cliVersion]) });
      return next;
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
      return null;
    } finally {
      setBusy(null);
    }
  }, []);

  return {
    status,
    login,
    accounts,
    switchToAccount,
    removeSavedAccount,
    device,
    banner,
    error,
    busy,
    refresh,
    saveSettings,
    saveMihomo,
    saveForcedModel,
    probing,
    probingGroup,
    latency,
    probeLatency,
    selectMihomoNode,
    probeMihomoGroup,
    probeAllMihomo,
    setChainSystemProxy,
    setLanAccess,
    rotateLanApiKey,
    saveVmIdentity,
    regenerateVmInstallation,
    detectVmVersion,
    startLogin,
    importRefreshLogin,
    importAccessLogin,
    cancelLogin,
    openLoginPage,
    loadLogin,
    dismissBanner: () => setBanner(null),
  };
}
