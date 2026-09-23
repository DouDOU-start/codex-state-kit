export interface LogEntry {
  statePolicy?: StateMissPolicy | null;
  accountId?: string | null;
  accountEmail?: string | null;
  id: number;
  ts: string;
  method: string;
  path: string;
  status: number;
  /** Compatibility alias for responseHeaderMs. */
  ms: number;
  responseHeaderMs?: number | null;
  responseContentEncoding?: string | null;
  firstTokenMs?: number | null;
  outputTokens?: number | null;
  tokensPerSecond?: number | null;
  inProgress?: boolean;
  flow: string;
  transport: string;
  targetOrigin: string;
  finalOrigin?: string | null;
  routeKind: string;
  proxyEndpoint?: string | null;
  proxySession?: string | null;
  peerAddr?: string | null;
  httpVersion?: string | null;
  model?: string | null;
  upstreamResponseModel?: string | null;
  contentEncoding: string;
  bodyBytes: number;
  turnStateAction: string;
  turnStateLen?: number | null;
  returnedTurnStateLen?: number | null;
  errorKind?: string | null;
  streamState: "not_tracked" | "awaiting_first_chunk" | "streaming" | "completed" | "error" | "cancelled" | string;
  firstChunkMs?: number | null;
  lastChunkMs?: number | null;
  streamTotalMs?: number | null;
  streamBytes: number;
  streamChunks: number;
  maxIdleMs?: number | null;
  currentIdleMs?: number | null;
}

export interface TokenLenCount {
  len: number;
  count: number;
}

export interface PoolTokenInfo {
  len: number;
  ageSecs: number;
  isBound: boolean;
  isValid: boolean;
}

export interface ModelTokenView {
  model: string;
  status: "active" | "refreshing" | "expired" | "empty" | string;
  ageSecs?: number | null;
  len?: number | null;
  capturedAt?: string | null;
  distribution?: TokenLenCount[];
  poolTokens?: PoolTokenInfo[];
  /** 模型级绑定覆盖（null/undefined 表示跟随全局） */
  boundOverride?: number | null;
  sharedFromModel?: string | null;
}

export interface TurnStateView {
  status: "idle" | "active" | "partial" | "empty" | string;
  ageSecs?: number | null;
  len?: number | null;
  source?: string | null;
  capturedAt?: string | null;
  models?: ModelTokenView[];
  boundTokenLen?: number;
  sharedSourceModel?: string | null;
  boundProxySession?: string | null;
}

export interface Status {
  tokenReusePolicy: TokenReusePolicy;
  stateFetchModel: string;
  tokenFetchPaused?: boolean;
  tokenMaxAgeMins?: number;
  tokenPrefetchAgeMins?: number;
  diagLogPath?: string;
  forcedModel: string;
  stateMissPolicy: StateMissPolicy;
  configuredModels: string[];
  currentAccountId?: string | null;
  currentAccountEmail?: string | null;
  accountTraffic: { concurrentRequests: number; rpm: number };
  proxyListen: string;
  upstream: string;
  codexHome: string;
  proxyOk: boolean;
  attached: boolean;
  proxyError?: string | null;
  attachError?: string | null;
  outboundProxy: string;
  outboundMode: OutboundMode;
  mihomoSubscription: string;
  mihomoNode: string;
  mihomo: MihomoStatus;
  fetchError?: string | null;
  fetchOkAt?: string | null;
  turnState: TurnStateView;
  degraded: boolean;
  degradedAt?: string | null;
  logs: LogEntry[];
  wsUpstreamEnabled: boolean;
  wsUpstreamConnected: boolean;
  wsUpstreamConnectedAt?: string | null;
}

export interface SettingsPatch {
  tokenReusePolicy: TokenReusePolicy;
  stateFetchModel: string;
  tokenFetchPaused?: boolean;
  tokenMaxAgeMins?: number;
  tokenPrefetchAgeMins?: number;
  forcedModel: string;
  models: string[];
  stateMissPolicy: StateMissPolicy;
  proxyListen: string;
  upstream: string;
  codexHome: string;
  outboundProxy: string;
  outboundMode: OutboundMode;
  mihomoSubscription: string;
  mihomoNode: string;
  wsUpstreamEnabled?: boolean;
}

export type TokenReusePolicy = "shared_292" | "per_model";
export type OutboundMode = "manual" | "mihomo";
export type ProbeKind = "manual" | "mihomo";
export type StateMissPolicy = "preserve" | "wait" | "strip" | "passthrough" | "strip_all";

export interface LatencySample {
  name: string;
  delayMs: number | null;
  error: string | null;
}

export interface LatencyReport {
  target: string;
  samples: LatencySample[];
}

export interface ProxyGroupNode {
  name: string;
  nodeType: string;
  delay: number | null;
  udp: boolean;
}

export interface ProxyGroup {
  name: string;
  groupType: "select" | "url-test" | "relay" | "fallback" | "load-balance" | string;
  now: string | null;
  all: ProxyGroupNode[];
}

export interface MihomoStatus {
  available: boolean;
  phase: string;
  proxyUrl: string | null;
  selected: string | null;
  nodes: string[];
  groups: ProxyGroup[];
  error: string | null;
}

export interface ProviderView {
  name: string;
  baseUrl?: string | null;
}

export interface CodexConfigView {
  codexHome: string;
  modelProvider?: string | null;
  openaiBaseUrl?: string | null;
  providers: ProviderView[];
  suggestedBaseUrl: string;
  attached: boolean;
  error?: string | null;
}

export interface LoginStatus {
  loggedIn: boolean;
  authMode?: string | null;
  email?: string | null;
  accountId?: string | null;
  refreshable: boolean;
}

export type LoginMethod = "device" | "browser";
export type LoginMode = LoginMethod | "refresh" | "access";

export interface LoginStart {
  method: LoginMethod;
  userCode: string;
  verificationUri: string;
  expiresIn: number;
  interval: number;
}

export interface LoginPoll {
  status: "pending" | "ok" | "denied" | "expired" | "error" | string;
  message?: string | null;
  login?: LoginStatus | null;
}

export interface ActionResult {
  ok: boolean;
  message: string;
}

export interface Banner {
  kind: "ok" | "error";
  text: string;
}

/**
 * A persisted, per ChatGPT account usage total.  Costs are represented as
 * integer nano-dollars by the Rust billing store so the renderer never has to
 * accumulate floating point values.
 */
export interface BillingUsageTotals {
  requestCount: number;
  measuredRequestCount: number;
  unknownUsageCount: number;
  inputTokens: number;
  cachedInputTokens: number;
  outputTokens: number;
  costNanos: number | null;
}

export interface BillingAccountSummary {
  provider: string;
  accountId: string;
  email?: string | null;
  firstSeenAt?: string | null;
  lastSeenAt?: string | null;
  total: BillingUsageTotals;
  business: BillingUsageTotals;
  internal: BillingUsageTotals;
}

export interface BillingSummary {
  generatedAt: string;
  from?: string | null;
  to?: string | null;
  accounts: BillingAccountSummary[];
}

export type BillingRecordState = "pending" | "measured" | "missing_usage" | "interrupted" | string;
export type BillingRecordSource = "business" | "token_fetch" | "reverify" | string;

export interface BillingRecord {
  requestId: string;
  provider?: string;
  accountId: string;
  email?: string | null;
  source: BillingRecordSource;
  startedAt: string;
  finishedAt?: string | null;
  state: BillingRecordState;
  httpStatus?: number | null;
  requestedModel?: string | null;
  sentModel?: string | null;
  responseModel?: string | null;
  inputTokens?: number | null;
  cachedInputTokens?: number | null;
  outputTokens?: number | null;
  usageSource?: string | null;
  pricingRuleId?: number | null;
  costNanos?: number | null;
  currency?: string | null;
  errorKind?: string | null;
}

export interface BillingQuery {
  accountId?: string | null;
  from?: string | null;
  to?: string | null;
  source?: BillingRecordSource | null;
  model?: string | null;
  limit?: number;
  offset?: number;
}

export interface BillingRecordsPage {
  records: BillingRecord[];
  total: number;
  limit: number;
  offset: number;
}
