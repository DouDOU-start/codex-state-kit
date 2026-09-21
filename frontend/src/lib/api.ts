import { invoke } from "@tauri-apps/api/core";
import type {
  ActionResult,
  CodexConfigView,
  LoginPoll,
  LoginMethod,
  LoginStart,
  LoginStatus,
  SettingsPatch,
  Status,
} from "@/types";

export const isTauri = "__TAURI_INTERNALS__" in window;
export const GITHUB_REPO_URL = "https://github.com/DouDOU-start/codex-state-kit";

export async function openGithubRepo(): Promise<void> {
  await invoke<void>("open_github_repo");
}

const emptyTurnState = () => ({
  status: "empty",
  ageSecs: null,
  len: null,
  source: null,
  capturedAt: null,
});

const defaultStatus = (): Status => ({
  stateMissPolicy: "preserve",
  tokenReusePolicy: "shared_292",
  networkRoutePolicy: "same_network",
  forcedModel: "",
  configuredModels: [],
  currentAccountId: "mock-account-b",
  currentAccountEmail: "mock@example.com",
  accountTraffic: { concurrentRequests: 2, rpm: 18 },
  proxyListen: "127.0.0.1:8787",
  upstream: "https://chatgpt.com/backend-api/codex",
  codexHome: "~/.codex",
  proxyOk: true,
  attached: true,
  proxyError: null,
  attachError: null,
  outboundProxy: "socks5://proxy.example.test:44445",
  upstreamProxy: "socks5://proxy.example.test:44445",
  outboundMode: "manual",
  warpHttp2: false,
  warp: {
    available: true,
    registered: true,
    phase: "stopped",
    proxyUrl: null,
    exitIp: null,
    country: null,
    error: null,
  },
  fetchError: null,
  fetchOkAt: null,
  turnState: emptyTurnState(),
  degraded: false,
  degradedAt: null,
  logs: [{
    id: 1,
    accountId: "mock-account-a",
    accountEmail: "previous@example.com",
    ts: new Date(Date.now() - 900).toISOString(),
    method: "POST",
    path: "/responses",
    status: 200,
    ms: 263,
    responseHeaderMs: 263,
    flow: "token_fetch",
    transport: "http_sse",
    targetOrigin: "https://chatgpt.com:443",
    finalOrigin: "https://chatgpt.com:443",
    routeKind: "manual_proxy",
    proxyEndpoint: "socks5h://proxy.example.test:44445",
    peerAddr: "198.51.100.10:44445",
    httpVersion: "HTTP/2",
    model: "gpt-6-astra",
    contentEncoding: "json",
    bodyBytes: 142,
    turnStateAction: "captured",
    turnStateLen: null,
    returnedTurnStateLen: 292,
    errorKind: null,
    streamState: "not_tracked",
    firstChunkMs: null,
    lastChunkMs: null,
    streamTotalMs: null,
    streamBytes: 0,
    streamChunks: 0,
    maxIdleMs: null,
    currentIdleMs: null,
  }, ...[
    { model: "gpt-6-astra", upstreamResponseModel: null },
    { model: "gpt-6-astra", upstreamResponseModel: "gpt-6-astra" },
    { model: "gpt-5.6-sol", upstreamResponseModel: "gpt-6-sol" },
  ].map((models, index) => ({
    id: index + 2,
    accountId: "mock-account-b",
    accountEmail: "mock@example.com",
    ts: new Date().toISOString(),
    method: "POST",
    path: "/backend-api/codex/responses",
    status: 200,
    ms: 12640,
    responseHeaderMs: 184,
    firstTokenMs: 1240,
    outputTokens: 684,
    tokensPerSecond: 60,
    inProgress: false,
    flow: "business",
    transport: "http_sse",
    targetOrigin: "https://chatgpt.com:443",
    finalOrigin: "https://chatgpt.com:443",
    routeKind: "explicit_proxy",
    proxyEndpoint: "socks5h://proxy.example.test:44445",
    peerAddr: "198.51.100.10:44445",
    httpVersion: "HTTP/2",
    ...models,
    contentEncoding: "zstd",
    bodyBytes: 18432,
    turnStateAction: "replaced",
    turnStateLen: 292,
    returnedTurnStateLen: 292,
    errorKind: null,
    streamState: "completed",
    firstChunkMs: 321,
    lastChunkMs: 12_384,
    streamTotalMs: 12640,
    streamBytes: 64_218,
    streamChunks: 48,
    maxIdleMs: 306,
    currentIdleMs: null,
  }))],
});

const defaultLogin = (): LoginStatus => ({
  loggedIn: true,
  authMode: "chatgpt",
  email: "mock@example.com",
  accountId: "mock-account-b",
  refreshable: true,
});

let mockStatus = defaultStatus();
let mockLogin = defaultLogin();
let mockConfig: CodexConfigView = {
  codexHome: mockStatus.codexHome,
  modelProvider: null,
  openaiBaseUrl: null,
  providers: [],
  suggestedBaseUrl: `http://${mockStatus.proxyListen}`,
  attached: true,
};

function cloneStatus(): Status {
  return {
    ...mockStatus,
    warp: { ...mockStatus.warp },
    accountTraffic: { ...mockStatus.accountTraffic },
    turnState: { ...mockStatus.turnState },
    logs: mockStatus.logs.map((entry) => ({ ...entry })),
  };
}

export async function getStatus(): Promise<Status> {
  return isTauri ? invoke<Status>("get_status") : cloneStatus();
}

export async function setConfig(settings: SettingsPatch): Promise<Status> {
  if (isTauri) {
    return invoke<Status>("set_config", { settings });
  }
  const tokenRouteChanged = settings.outboundMode !== mockStatus.outboundMode || settings.warpHttp2 !== mockStatus.warpHttp2 || settings.outboundProxy !== mockStatus.outboundProxy || settings.codexHome !== mockStatus.codexHome || settings.upstream !== mockStatus.upstream;
  if (tokenRouteChanged && (settings.outboundMode === "manual" || settings.warpHttp2 !== mockStatus.warpHttp2)) {
    await stopWarp();
  }
  if (tokenRouteChanged) {
    mockStatus.turnState = emptyTurnState();
  }
  mockStatus = {
    ...mockStatus,
    proxyListen: settings.proxyListen,
    upstream: settings.upstream,
    codexHome: settings.codexHome,
    outboundProxy: settings.outboundProxy,
    upstreamProxy: settings.upstreamProxy,
    outboundMode: settings.outboundMode,
    warpHttp2: settings.warpHttp2,
    stateMissPolicy: settings.stateMissPolicy,
    tokenReusePolicy: settings.tokenReusePolicy,
    networkRoutePolicy: settings.networkRoutePolicy,
    forcedModel: settings.forcedModel,
    configuredModels: settings.models,
    proxyOk: true,
  };
  mockConfig.codexHome = settings.codexHome;
  mockConfig.suggestedBaseUrl = `http://${settings.proxyListen}`;
  if (tokenRouteChanged && settings.outboundMode === "warp") await connectWarp(true);
  return cloneStatus();
}

export async function getCodexConfig(home?: string): Promise<CodexConfigView> {
  if (isTauri) {
    return invoke<CodexConfigView>("get_codex_config", { home: home ?? null });
  }
  return {
    ...mockConfig,
    codexHome: home ?? mockConfig.codexHome,
    attached: mockStatus.attached,
    suggestedBaseUrl: `http://${mockStatus.proxyListen}`,
    providers: mockConfig.providers.map((provider) => ({ ...provider })),
  };
}

export async function refreshTurnState(): Promise<Status> {
  if (isTauri) {
    return invoke<Status>("refresh_turn_state");
  }
  const ready = mockStatus.outboundMode === "warp" ? mockStatus.warp.phase === "connected" : Boolean(mockStatus.outboundProxy);
  mockStatus = {
    ...mockStatus,
    fetchError: ready ? null : "出站代理尚未就绪",
    fetchOkAt: ready ? new Date().toISOString() : null,
    turnState: ready
      ? {
          status: "active",
          ageSecs: 12,
          len: 292,
          source: "fetch",
          capturedAt: new Date().toISOString(),
        }
      : emptyTurnState(),
  };
  return cloneStatus();
}

export async function setBoundTokenLen(len: number | null): Promise<Status> {
  if (isTauri) {
    return invoke<Status>("set_bound_token_len", { len });
  }
  return cloneStatus();
}

export async function setModelBoundTokenLen(model: string, len: number | null): Promise<Status> {
  if (isTauri) {
    return invoke<Status>("set_model_bound_token_len", { model, len });
  }
  return cloneStatus();
}

export async function connectWarp(acceptTerms: boolean): Promise<Status> {
  if (isTauri) return invoke<Status>("connect_warp", { acceptTerms });
  if (!mockStatus.warp.registered && !acceptTerms) throw new Error("请先同意 Cloudflare 服务条款");
  mockStatus.warp = { available: true, registered: true, phase: "connected", proxyUrl: "socks5h://127.0.0.1:40000", exitIp: "192.0.2.1", country: null, error: null };
  return cloneStatus();
}

export async function stopWarp(): Promise<Status> {
  if (isTauri) return invoke<Status>("stop_warp");
  mockStatus.warp = { ...mockStatus.warp, phase: "stopped", proxyUrl: null, exitIp: null, country: null, error: null };
  return cloneStatus();
}

export async function openWarpTerms(): Promise<void> {
  if (isTauri) return invoke<void>("open_warp_terms");
  window.open("https://www.cloudflare.com/application/terms/", "_blank", "noopener,noreferrer");
}

export async function getLoginStatus(home?: string): Promise<LoginStatus> {
  if (isTauri) {
    return invoke<LoginStatus>("get_login_status", { home: home ?? null });
  }
  return { ...mockLogin };
}

export async function importChatgptRefreshToken(
  home: string | undefined,
  refreshToken: string,
): Promise<LoginStatus> {
  if (isTauri) {
    return invoke<LoginStatus>("import_chatgpt_refresh_token", {
      home: home ?? null,
      refreshToken,
    });
  }
  mockLogin = defaultLogin();
  return { ...mockLogin };
}

export async function importChatgptAccessToken(
  home: string | undefined,
  accessToken: string,
): Promise<LoginStatus> {
  if (isTauri) {
    return invoke<LoginStatus>("import_chatgpt_access_token", {
      home: home ?? null,
      accessToken,
    });
  }
  mockLogin = {
    ...defaultLogin(),
    authMode: "chatgptAuthTokens",
    refreshable: false,
  };
  return { ...mockLogin };
}

let mockLoginPolls = 0;

export async function startChatgptLogin(home?: string, method: LoginMethod = "browser"): Promise<LoginStart> {
  if (isTauri) {
    return invoke<LoginStart>("start_chatgpt_login", { home: home ?? null, method });
  }
  mockLoginPolls = 0;
  return {
    method,
    userCode: method === "device" ? "MOCK-CODE" : "",
    verificationUri: "https://auth.openai.com/codex/device",
    expiresIn: 900,
    interval: 1,
  };
}

export async function pollChatgptLogin(): Promise<LoginPoll> {
  if (isTauri) {
    return invoke<LoginPoll>("poll_chatgpt_login");
  }
  if (++mockLoginPolls < 8) return { status: "pending" };
  mockLogin = defaultLogin();
  return { status: "ok", message: "预览：已模拟登录成功", login: mockLogin };
}

export async function cancelChatgptLogin(): Promise<ActionResult> {
  if (isTauri) {
    return invoke<ActionResult>("cancel_chatgpt_login");
  }
  return { ok: true, message: "已取消登录" };
}

export async function openUrl(url: string): Promise<ActionResult> {
  if (isTauri) {
    return invoke<ActionResult>("open_url", { url });
  }
  return { ok: true, message: "预览模式不发起真实授权" };
}
