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
  openWarpTerms as openWarpTermsApi,
} from "@/lib/api";
import type { Banner, LoginMethod, LoginStart, LoginStatus, Status, OutboundMode, StateMissPolicy } from "@/types";

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
  const [busy, setBusy] = useState<"login" | "save" | "refresh" | "warp" | null>(null);
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

  const persistSettings = useCallback(async (home: string, outboundProxy: string, current: Status, outboundMode = current.outboundMode, warpHttp2 = current.warpHttp2, upstreamProxy = current.upstreamProxy) => {
    if (home === current.codexHome && outboundProxy === current.outboundProxy && outboundMode === current.outboundMode && warpHttp2 === current.warpHttp2 && upstreamProxy === current.upstreamProxy) return current;
    return setConfig({
      proxyListen: current.proxyListen,
      upstream: current.upstream,
      codexHome: home,
      outboundProxy,
      upstreamProxy,
      outboundMode,
      warpHttp2,
      stateMissPolicy: current.stateMissPolicy,
      models: current.configuredModels,
    });
  }, []);

  const saveSettings = useCallback(async (home: string, outboundProxy: string, outboundMode?: OutboundMode, warpHttp2?: boolean, upstreamProxy?: string) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      if (home.trim() !== latest.codexHome) {
        stopPolling();
        setDevice(null);
        await cancelChatgptLogin();
      }
      const next = await persistSettings(home.trim(), outboundProxy, latest, outboundMode, warpHttp2, upstreamProxy?.trim());
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

  const openWarpTerms = useCallback(async () => {
    try { await openWarpTermsApi(); }
    catch (cause) { setBanner({ kind: "error", text: errorMessage(cause) }); }
  }, []);

  const setStateMissPolicy = useCallback(async (stateMissPolicy: StateMissPolicy) => {
    setBusy("save");
    try {
      const latest = await getStatus();
      const next = await setConfig({
        proxyListen: latest.proxyListen, upstream: latest.upstream, codexHome: latest.codexHome,
        outboundProxy: latest.outboundProxy, upstreamProxy: latest.upstreamProxy,
        outboundMode: latest.outboundMode, warpHttp2: latest.warpHttp2, stateMissPolicy,
        models: latest.configuredModels,
      });
      setStatus(next);
      setBanner({ kind: "ok", text: "State 处理策略已保存。" });
    } catch (cause) {
      setBanner({ kind: "error", text: errorMessage(cause) });
    } finally {
      setBusy(null);
    }
  }, []);

  const refreshToken = useCallback(async (home: string, outboundProxy: string) => {
    setBusy("refresh");
    try {
      const latest = status ?? (await getStatus());
      await persistSettings(home, outboundProxy, latest);
      const next = await refreshTurnState();
      setStatus(next);
      setBanner({ kind: "ok", text: "已通过出站代理刷新 turn-state。" });
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
  }, [persistSettings, status]);

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
    setStateMissPolicy,
    refreshToken,
    openWarpTerms,
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
