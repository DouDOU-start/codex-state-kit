import { invoke } from "@tauri-apps/api/core";
import type {
  ActionResult,
  CodexConfigView,
  LoginPoll,
  LoginMethod,
  LoginStart,
  LoginStatus,
  ProbeKind,
  SettingsPatch,
  Status,
  VmProfile,
  LatencyReport,
  LatencySample,
  BillingRecord,
  BillingQuery,
  BillingRecordsPage,
  BillingSummary,
  BillingUsageTotals,
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
  stateFetchModel: "",
  tokenFetchPaused: false,
  tokenMaxAgeMins: 40,
  tokenPrefetchAgeMins: 35,
  diagLogPath: "",
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
  outboundMode: "manual",
  mihomoSubscription: "",
  mihomoNode: "",
  mihomo: {
    available: false,
    phase: "stopped",
    proxyUrl: null,
    selected: null,
    nodes: [],
    groups: [],
    error: null,
  },
  fetchError: null,
  fetchOkAt: null,
  turnState: emptyTurnState(),
  degraded: false,
  degradedAt: null,
  vmIdentity: {
    installationId: "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    sessionId: "11111111-2222-4333-8444-555555555555",
    cliVersion: "0.155.0",
    originator: "codex_cli_rs",
    osType: "Mac OS",
    osVersion: "15.5.0",
    arch: "arm64",
    terminal: "xterm-256color",
    userAgent: "codex_cli_rs/0.155.0 (Mac OS 15.5.0; arm64) xterm-256color",
    versionLocked: false,
  },
  wsUpstreamEnabled: true,
  wsUpstreamConnected: false,
  wsUpstreamConnectedAt: null,
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
  })), {
    id: 5,
    accountId: "mock-account-b",
    accountEmail: "mock@example.com",
    ts: new Date(Date.now() - 400).toISOString(),
    method: "POST",
    path: "/responses",
    status: 200,
    ms: 2140,
    responseHeaderMs: 2140,
    flow: "token_fetch",
    transport: "http_sse",
    targetOrigin: "https://chatgpt.com:443",
    finalOrigin: "https://chatgpt.com:443",
    routeKind: "manual_proxy",
    proxyEndpoint: "socks5h://proxy.example.test:44445",
    peerAddr: "198.51.100.10:44445",
    httpVersion: "HTTP/1.1",
    model: "gpt-5.6-luna",
    contentEncoding: "json",
    bodyBytes: 156,
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
  }],
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
    mihomo: {
      ...mockStatus.mihomo,
      nodes: [...mockStatus.mihomo.nodes],
      groups: mockStatus.mihomo.groups.map((group) => ({
        ...group,
        all: group.all.map((node) => ({ ...node })),
      })),
    },
    accountTraffic: { ...mockStatus.accountTraffic },
    turnState: { ...mockStatus.turnState },
    vmIdentity: { ...mockStatus.vmIdentity },
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
  const tokenRouteChanged = settings.outboundMode !== mockStatus.outboundMode || settings.outboundProxy !== mockStatus.outboundProxy || settings.codexHome !== mockStatus.codexHome || settings.upstream !== mockStatus.upstream || settings.mihomoSubscription !== mockStatus.mihomoSubscription || settings.mihomoNode !== mockStatus.mihomoNode;
  if (tokenRouteChanged) {
    mockStatus.turnState = emptyTurnState();
  }
  const selected = settings.mihomoNode || "node-a";
  const nodes = settings.mihomoNode ? [settings.mihomoNode, "node-b"] : ["node-a", "node-b"];
  mockStatus = {
    ...mockStatus,
    proxyListen: settings.proxyListen,
    upstream: settings.upstream,
    codexHome: settings.codexHome,
    outboundProxy: settings.outboundProxy,
    outboundMode: settings.outboundMode,
    mihomoSubscription: settings.mihomoSubscription,
    mihomoNode: settings.mihomoNode,
    mihomo: settings.outboundMode === "mihomo"
      ? {
          available: true,
          phase: "connected",
          proxyUrl: "http://127.0.0.1:52190",
          selected,
          nodes,
          groups: [{
            name: "Kit",
            groupType: "select",
            now: selected,
            all: nodes.map((name) => ({ name, nodeType: "ss", delay: null, udp: true })),
          }],
          error: null,
        }
      : { ...mockStatus.mihomo, phase: "stopped", proxyUrl: null, selected: null, groups: [], error: null },
    stateMissPolicy: settings.stateMissPolicy,
    tokenReusePolicy: settings.tokenReusePolicy,
    stateFetchModel: settings.stateFetchModel,
    tokenFetchPaused: settings.tokenFetchPaused ?? false,
    tokenMaxAgeMins: settings.tokenMaxAgeMins ?? 40,
    tokenPrefetchAgeMins: settings.tokenPrefetchAgeMins ?? 35,
    forcedModel: settings.forcedModel,
    configuredModels: settings.models,
    wsUpstreamEnabled: settings.wsUpstreamEnabled !== false,
    proxyOk: true,
  };
  mockConfig.codexHome = settings.codexHome;
  mockConfig.suggestedBaseUrl = `http://${settings.proxyListen}`;
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
  const ready = mockStatus.outboundMode === "mihomo"
      ? mockStatus.mihomo.phase === "connected"
      : Boolean(mockStatus.outboundProxy);
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

export async function probeOutboundLatency(kind: ProbeKind, proxy?: string): Promise<LatencyReport> {
  if (isTauri) return invoke<LatencyReport>("probe_outbound_latency", { kind, proxy: proxy ?? null });
  await new Promise((resolve) => window.setTimeout(resolve, 500));
  const target = "https://chatgpt.com/backend-api/codex";
  if (kind === "mihomo") {
    const names = mockStatus.mihomo.nodes.length ? mockStatus.mihomo.nodes : ["node-a", "node-b"];
    return {
      target,
      samples: names.map((name, index) => ({
        name,
        delayMs: index === names.length - 1 && names.length > 1 ? null : 120 + index * 80,
        error: index === names.length - 1 && names.length > 1 ? "超时" : null,
      })),
    };
  }
  if (kind === "manual" && !proxy?.trim() && !mockStatus.outboundProxy.trim()) throw new Error("请先填写代理地址");
  return { target, samples: [{ name: kind, delayMs: 128, error: null }] };
}

export async function mihomoSelect(group: string, node: string): Promise<void> {
  if (isTauri) return invoke<void>("mihomo_select", { group, node });
  const target = mockStatus.mihomo.groups.find((item) => item.name === group);
  if (target?.groupType === "select") target.now = node;
  mockStatus.mihomo.selected = node;
}

export async function mihomoGroupDelay(group: string): Promise<LatencySample[]> {
  if (isTauri) return invoke<LatencySample[]>("mihomo_group_delay", { group });
  await new Promise((resolve) => window.setTimeout(resolve, 300));
  const target = mockStatus.mihomo.groups.find((item) => item.name === group);
  const names = target?.all.map((node) => node.name) ?? mockStatus.mihomo.nodes;
  return names.map((name, index) => {
    const delayMs = index === names.length - 1 && names.length > 1 ? null : 90 + index * 40;
    const node = target?.all.find((item) => item.name === name);
    if (node) node.delay = delayMs;
    return { name, delayMs, error: delayMs == null ? "超时" : null };
  });
}

function mockUserAgent(profile: VmProfile): string {
  return `${profile.originator}/${profile.cliVersion} (${profile.osType} ${profile.osVersion}; ${profile.arch}) ${profile.terminal}`;
}

export async function updateVmIdentity(profile: VmProfile): Promise<Status> {
  if (isTauri) return invoke<Status>("update_vm_identity", { profile });
  mockStatus = {
    ...mockStatus,
    vmIdentity: {
      ...mockStatus.vmIdentity,
      ...profile,
      userAgent: mockUserAgent(profile),
      versionLocked: true,
    },
  };
  return cloneStatus();
}

export async function regenerateVmInstallationId(): Promise<Status> {
  if (isTauri) return invoke<Status>("regenerate_vm_installation_id");
  mockStatus = {
    ...mockStatus,
    vmIdentity: {
      ...mockStatus.vmIdentity,
      installationId: crypto.randomUUID(),
    },
  };
  return cloneStatus();
}

export async function detectVmCliVersion(): Promise<Status> {
  if (isTauri) return invoke<Status>("detect_vm_cli_version");
  const cliVersion = "0.160.0";
  const profile: VmProfile = { ...mockStatus.vmIdentity, cliVersion };
  mockStatus = {
    ...mockStatus,
    vmIdentity: {
      ...mockStatus.vmIdentity,
      cliVersion,
      userAgent: mockUserAgent(profile),
      versionLocked: false,
    },
  };
  return cloneStatus();
}

export async function reconnectWsUpstream(): Promise<Status> {
  if (isTauri) return invoke<Status>("ws_upstream_reconnect");
  mockStatus = {
    ...mockStatus,
    wsUpstreamConnected: false,
    wsUpstreamConnectedAt: null,
  };
  return cloneStatus();
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

const emptyBillingTotals = (): BillingUsageTotals => ({
  requestCount: 0,
  measuredRequestCount: 0,
  unknownUsageCount: 0,
  inputTokens: 0,
  cachedInputTokens: 0,
  outputTokens: 0,
  costNanos: null,
});

const mockBillingSummary = (): BillingSummary => {
  const previous = emptyBillingTotals();
  previous.requestCount = 1;
  previous.measuredRequestCount = 1;
  previous.inputTokens = 1_420;
  previous.outputTokens = 684;
  previous.costNanos = 1_240_000;
  const current = emptyBillingTotals();
  current.requestCount = 3;
  current.measuredRequestCount = 2;
  current.unknownUsageCount = 1;
  current.inputTokens = 12_480;
  current.cachedInputTokens = 2_048;
  current.outputTokens = 2_316;
  current.costNanos = 8_460_000;
  return {
    generatedAt: new Date().toISOString(),
    from: null,
    to: null,
    accounts: [
      {
        provider: "chatgpt",
        accountId: "mock-account-a",
        email: "previous@example.com",
        firstSeenAt: new Date(Date.now() - 86_400_000 * 9).toISOString(),
        lastSeenAt: new Date(Date.now() - 86_400_000).toISOString(),
        total: previous,
        business: previous,
        internal: emptyBillingTotals(),
      },
      {
        provider: "chatgpt",
        accountId: "mock-account-b",
        email: "mock@example.com",
        firstSeenAt: new Date(Date.now() - 86_400_000 * 3).toISOString(),
        lastSeenAt: new Date().toISOString(),
        total: current,
        business: current,
        internal: emptyBillingTotals(),
      },
    ],
  };
};

const mockBillingRecords: BillingRecord[] = [
  {
    requestId: "mock-billing-3",
    provider: "chatgpt",
    accountId: "mock-account-b",
    email: "mock@example.com",
    source: "business",
    startedAt: new Date(Date.now() - 20_000).toISOString(),
    finishedAt: new Date(Date.now() - 7_000).toISOString(),
    state: "measured",
    httpStatus: 200,
    requestedModel: "gpt-5.6-sol",
    sentModel: "gpt-6-sol",
    responseModel: "gpt-6-sol",
    inputTokens: 4_608,
    cachedInputTokens: 1_024,
    outputTokens: 1_024,
    usageSource: "provider_response",
    pricingRuleId: 1,
    costNanos: 4_220_000,
    currency: "USD",
  },
  {
    requestId: "mock-billing-2",
    provider: "chatgpt",
    accountId: "mock-account-b",
    email: "mock@example.com",
    source: "token_fetch",
    startedAt: new Date(Date.now() - 60_000).toISOString(),
    finishedAt: new Date(Date.now() - 45_000).toISOString(),
    state: "missing_usage",
    httpStatus: 200,
    requestedModel: "gpt-6-astra",
    sentModel: "gpt-6-astra",
    usageSource: null,
    pricingRuleId: null,
    costNanos: null,
    currency: "USD",
  },
  {
    requestId: "mock-billing-1",
    provider: "chatgpt",
    accountId: "mock-account-a",
    email: "previous@example.com",
    source: "business",
    startedAt: new Date(Date.now() - 86_400_000).toISOString(),
    finishedAt: new Date(Date.now() - 86_400_000 + 9_000).toISOString(),
    state: "measured",
    httpStatus: 200,
    requestedModel: "gpt-6-astra",
    sentModel: "gpt-6-astra",
    responseModel: "gpt-6-astra",
    inputTokens: 1_420,
    cachedInputTokens: 0,
    outputTokens: 684,
    usageSource: "provider_response",
    pricingRuleId: 1,
    costNanos: 1_240_000,
    currency: "USD",
  },
];

function cloneBillingSummary(summary: BillingSummary): BillingSummary {
  return {
    ...summary,
    accounts: summary.accounts.map((account) => ({
      ...account,
      total: { ...account.total },
      business: { ...account.business },
      internal: { ...account.internal },
    })),
  };
}

function cloneBillingRecord(record: BillingRecord): BillingRecord {
  return { ...record };
}

export async function getBillingSummary(period: Pick<BillingQuery, "from" | "to"> = {}): Promise<BillingSummary> {
  if (isTauri) {
    return invoke<BillingSummary>("get_billing_summary", {
      from: period.from ?? null,
      to: period.to ?? null,
    });
  }
  return cloneBillingSummary(mockBillingSummary());
}

export async function getBillingRecords(query: BillingQuery = {}): Promise<BillingRecordsPage> {
  if (isTauri) {
    return invoke<BillingRecordsPage>("get_billing_records", {
      accountId: query.accountId ?? null,
      from: query.from ?? null,
      to: query.to ?? null,
      source: query.source ?? null,
      model: query.model ?? null,
      limit: query.limit ?? 50,
      offset: query.offset ?? 0,
    });
  }
  const filtered = mockBillingRecords.filter((record) => {
    if (query.accountId && record.accountId !== query.accountId) return false;
    if (query.source && record.source !== query.source) return false;
    if (query.model && record.sentModel !== query.model && record.requestedModel !== query.model) return false;
    return true;
  });
  const limit = query.limit ?? 50;
  const offset = query.offset ?? 0;
  return {
    records: filtered.slice(offset, offset + limit).map(cloneBillingRecord),
    total: filtered.length,
    limit,
    offset,
  };
}
