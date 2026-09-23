import { useCallback, useEffect, useRef, useState } from "react";
import {
  cancelChatgptLogin,
  getLoginStatus,
  getStatus,
  importChatgptAccessToken,
  importChatgptRefreshToken,
  pollChatgptLogin,
  refreshTurnState,
  setConfig,
  setBoundTokenLen,
  setModelBoundTokenLen,
  openUrl,
  startChatgptLogin,
  probeOutboundLatency,
  mihomoSelect,
  mihomoGroupDelay,
  reconnectWsUpstream,
} from "@/lib/api";
import type { Banner, LoginMethod, LoginStart, LoginStatus, Status, OutboundMode, StateMissPolicy, TokenReusePolicy, SettingsPatch, ProbeKind, LatencyReport } from "@/types";

function patchFrom(status: Status, overrides: Partial<SettingsPatch> = {}): SettingsPatch {
  return {
    proxyListen: status.proxyListen,
    upstream: status.upstream,
    codexHome: status.codexHome,
    outboundProxy: status.outboundProxy,
    outboundMode: status.outboundMode,
    mihomoSubscription: status.mihomoSubscription ?? "",
    mihomoNode: status.mihomoNode ?? "",
    stateMissPolicy: status.stateMissPolicy,
    tokenReusePolicy: status.tokenReusePolicy,
    stateFetchModel: status.stateFetchModel ?? "",
    tokenFetchPaused: status.tokenFetchPaused ?? false,
    tokenMaxAgeMins: status.tokenMaxAgeMins ?? 40,
    tokenPrefetchAgeMins: status.tokenPrefetchAgeMins ?? 35,
    forcedModel: status.forcedModel ?? "",
    models: status.configuredModels,
    wsUpstreamEnabled: status.wsUpstreamEnabled !== false,
    ...overrides,
  };
}

function errorMessage(cause: unknown): string {
  if (typeof cause === "string") return cause;
  if (cause instanceof Error) return cause.message;
  if (cause && typeof cause === "object" && "message" in cause) {
    return String((cause as { message: unknown }).message);
  }
  return String(cause);
}

export function useCodexStateKit() {
  const [status, setStatus] = useState<Status | null>(null);
  const [login, setLogin] = useState<LoginStatus | null>(null);
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
    const timer = window.setInterval(() => void loadStatus(true), 1500);
    return () => window.clearInterval(timer);
  }, [loadStatus, refresh]);

  useEffect(() => {
    if (!status) return;
    void loadLogin(status.codexHome);
  }, [loadLogin, status?.codexHome]);

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

  const setStateMissPolicy = useCallback(async (stateMissPolicy: StateMissPolicy) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig(patchFrom(latest, { stateMissPolicy }));
      setStatus(next);
      setBanner({ kind: "ok", text: "State 处理策略已保存。" });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const setTokenReusePolicy = useCallback(async (tokenReusePolicy: TokenReusePolicy) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig(patchFrom(latest, { tokenReusePolicy }));
      setStatus(next);
      setBanner({ kind: "ok", text: tokenReusePolicy === "shared_292"
        ? "已启用跨模型复用 292，同账号共享有效票据。"
        : "已恢复按模型独立，各模型分别获取和复用 Token。" });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const saveStateFetchModel = useCallback(async (stateFetchModel: string) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig(patchFrom(latest, { stateFetchModel: stateFetchModel.trim() }));
      setStatus(next);
      setBanner({ kind: "ok", text: next.tokenReusePolicy === "shared_292" && next.stateFetchModel
        ? `跨模型复用只使用 ${next.stateFetchModel} 获取 292。`
        : "已取消指定取票模型，292 会从可用模型中获取。" });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const setTokenFetchPaused = useCallback(async (tokenFetchPaused: boolean) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig(patchFrom(latest, { tokenFetchPaused }));
      setStatus(next);
      setBanner({ kind: "ok", text: tokenFetchPaused
        ? "已暂停获取 Token，后台不再打票。已缓存的 Token 仍可注入。"
        : "已继续获取 Token。" });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const saveForcedModel = useCallback(async (forcedModel: string) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig(patchFrom(latest, { forcedModel: forcedModel.trim() }));
      setStatus(next);
      setBanner({ kind: "ok", text: next.forcedModel
        ? `已强制绑定模型 ${next.forcedModel}，下游请求都会改成这个 ID 再转发。`
        : "已关闭强制绑定模型，按下游请求的模型 ID 转发。" });
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

  const probeMihomoGroup = useCallback(async (group: string) => {
    setProbingGroup(group);
    try {
      await mihomoGroupDelay(group);
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

  const refetchTurnState = useCallback(async () => {
    setBusy("refresh");
    try {
      const next = await refreshTurnState();
      setStatus(next);
      setBanner({ kind: "ok", text: "已重新获取 Token。" });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
      try {
        setStatus(await getStatus());
      } catch {
        // keep previous status
      }
    } finally {
      setBusy(null);
    }
  }, []);

  const startLogin = useCallback(async (home: string, method: LoginMethod) => {
    setBusy("login");
    stopPolling();
    const generation = loginGeneration.current;
    try {
      const started = await startChatgptLogin(home, method);
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
            setBanner({ kind: "ok", text: poll.message || "已登录 ChatGPT" });
          } else {
            setBanner({ kind: "error", text: poll.message || "登录失败" });
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

  const importRefreshLogin = useCallback(async (home: string, refreshToken: string) => {
    setBusy("login");
    stopPolling();
    setDevice(null);
    try {
      const loggedIn = await importChatgptRefreshToken(home.trim(), refreshToken.trim());
      setLogin(loggedIn);
      setBanner({ kind: "ok", text: "Refresh Token 已换取并同步到 Codex 账号" });
      return true;
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
      return false;
    } finally {
      setBusy(null);
    }
  }, [stopPolling]);

  const importAccessLogin = useCallback(async (home: string, accessToken: string) => {
    setBusy("login");
    stopPolling();
    setDevice(null);
    try {
      const loggedIn = await importChatgptAccessToken(home.trim(), accessToken.trim());
      setLogin(loggedIn);
      setBanner({ kind: "ok", text: "Access Token 已同步到 Codex 账号" });
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
      setBanner({ kind: "ok", text: "已取消登录" });
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

  const setWsUpstreamEnabled = useCallback(async (enabled: boolean) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig(patchFrom(latest, { wsUpstreamEnabled: enabled }));
      setStatus(next);
      setBanner({ kind: "ok", text: enabled ? "上游已改为 WebSocket，失败时回退 HTTP" : "上游已改回 HTTP" });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const reconnectUpstream = useCallback(async () => {
    setBusy("save");
    try {
      const next = await reconnectWsUpstream();
      setStatus(next);
      setBanner({ kind: "ok", text: "已断开上游 WebSocket，下次请求会重新连接" });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const bindTokenLen = useCallback(async (len: number | null) => {
    try {
      const next = await setBoundTokenLen(len);
      setStatus(next);
      setBanner({ kind: "ok", text: len ? `已全局绑定 ${len} Token` : "已恢复账号默认绑定" });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    }
  }, []);

  const bindModelTokenLen = useCallback(async (model: string, len: number | null) => {
    try {
      const next = await setModelBoundTokenLen(model, len);
      setStatus(next);
      setBanner({ kind: "ok", text: len ? `${model} 已绑定 ${len} Token` : `${model} 已恢复跟随全局` });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    }
  }, []);

  return {
    status,
    login,
    device,
    banner,
    error,
    busy,
    refresh,
    saveSettings,
    saveMihomo,
    setStateMissPolicy,
    setTokenReusePolicy,
    saveStateFetchModel,
    setTokenFetchPaused,
    saveForcedModel,
    refetchTurnState,
    probing,
    probingGroup,
    latency,
    probeLatency,
    selectMihomoNode,
    probeMihomoGroup,
    probeAllMihomo,
    setWsUpstreamEnabled,
    reconnectUpstream,
    startLogin,
    importRefreshLogin,
    importAccessLogin,
    cancelLogin,
    openLoginPage,
    loadLogin,
    bindTokenLen,
    bindModelTokenLen,
    dismissBanner: () => setBanner(null),
  };
}
