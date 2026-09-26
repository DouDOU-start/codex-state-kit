export interface LogEntry {
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
  errorKind?: string | null;
  errorMessage?: string | null;
  streamState: "not_tracked" | "awaiting_first_chunk" | "streaming" | "completed" | "error" | "cancelled" | string;
  firstChunkMs?: number | null;
  lastChunkMs?: number | null;
  streamTotalMs?: number | null;
  streamBytes: number;
  streamChunks: number;
  maxIdleMs?: number | null;
  currentIdleMs?: number | null;
}

/** The system a virtual device reports; version, arch and terminal follow it. */
export type DevicePlatform = "mac" | "windows" | "linux";

export interface VmIdentityView {
  enabled: boolean;
  environment?: VirtualEnvironment;
  installationId: string;
  sessionId: string;
  platform: DevicePlatform;
  cliVersion: string;
  originator: string;
  osType: string;
  osVersion: string;
  arch: string;
  terminal: string;
  terminalVersion: string;
  terminalMultiplexer: string;
  userAgent: string;
}

export interface VmProfile {
  platform: DevicePlatform;
  environment?: VirtualEnvironment;
  enabled?: boolean;
  terminal?: string;
  terminalVersion?: string;
  terminalMultiplexer?: string;
}

export interface VirtualEnvironment {
  autoRegion: boolean;
  timezone: string;
  locale: string;
  region: string;
}

export interface Status {
  diagLogPath?: string;
  forcedModel: string;
  currentAccountId?: string | null;
  currentAccountEmail?: string | null;
  accountTraffic: { concurrentRequests: number; rpm: number; tpm: number };
  proxyListen: string;
  /** Whether the proxy listens on all interfaces for LAN clients. */
  lanAccessEnabled: boolean;
  /** Whether a Kit gateway API key has been configured. */
  lanApiKeyConfigured: boolean;
  /** Masked gateway key for display; the plaintext key is never returned by status. */
  lanApiKeyMasked?: string | null;
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
  logs: LogEntry[];
  vmIdentity: VmIdentityView;
  chainSystemProxy?: boolean;
  systemProxy?: SystemProxyView;
  /** Latest downgraded request since Kit started. */
  lastDowngrade?: DowngradeEvent | null;
}

/** confirmed: upstream reported a different serving model (openai-model). */
export type DowngradeVerdict = "confirmed" | "suspected";

export interface DowngradeReport {
  verdict: DowngradeVerdict;
  requestedModel?: string | null;
  /** The model that served the turn instead, when known. */
  effectiveModel?: string | null;
  safetyBuffering: boolean;
  reasons: string[];
  useCases: string[];
  /** Faster model upstream offered for a retry while buffering. */
  fasterModel?: string | null;
  verifications?: string[];
  turnStateLen?: number | null;
  primaryUsedPercent?: number | null;
  encryptedMin?: number | null;
  /** Human-readable evidence, strongest first. */
  signals: string[];
}

export interface DowngradeEvent {
  requestId: string;
  at: string;
  accountId: string;
  email?: string | null;
  report: DowngradeReport;
}

export interface SystemProxyView {
  enabled: boolean;
  /** e.g. "HTTP 127.0.0.1:7897" */
  detected?: string | null;
  lastError?: string | null;
}

export interface SettingsPatch {
  forcedModel: string;
  proxyListen: string;
  lanAccessEnabled: boolean;
  upstream: string;
  codexHome: string;
  outboundProxy: string;
  outboundMode: OutboundMode;
  mihomoSubscription: string;
  mihomoNode: string;
  chainSystemProxy?: boolean;
}

export type OutboundMode = "manual" | "mihomo";
export type ProbeKind = "manual" | "mihomo" | "mihomo_codex";

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
  kind: "ok" | "warn" | "error";
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
  interruptedRequestCount?: number;
  pendingRequestCount?: number;
  missingUsageCount?: number;
  inputTokens: number;
  cachedInputTokens: number;
  cacheWriteTokens?: number;
  outputTokens: number;
  /** Already included in outputTokens. */
  reasoningTokens?: number;
  /** Null as soon as any request lacks a price. */
  costNanos: number | null;
  /** Sum over the requests that have a price. */
  pricedCostNanos: number;
  /** Requests without a price (no matching price or no usage reported). */
  unpricedCount: number;
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
  cacheWriteTokens?: number | null;
  outputTokens?: number | null;
  /** Already included in outputTokens. */
  reasoningTokens?: number | null;
  usageSource?: string | null;
  pricingRuleId?: number | null;
  /** Catalog key (or manual rule model) the cost was priced with. */
  pricingModel?: string | null;
  serviceTier?: ServiceTier | null;
  longContext?: boolean;
  inputCostNanos?: number | null;
  cacheReadCostNanos?: number | null;
  cacheWriteCostNanos?: number | null;
  outputCostNanos?: number | null;
  /** Persisted time to first visible output. */
  firstTokenMs?: number | null;
  /** http | http_sse | http_to_ws | ws_to_ws */
  transport?: string | null;
  downgrade?: DowngradeReport | null;
  costNanos?: number | null;
  currency?: string | null;
  errorKind?: string | null;
  errorMessage?: string | null;
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

export type ServiceTier = "standard" | "priority" | "flex";

/** USD per token. */
export interface PriceRates {
  input: number;
  cacheRead: number;
  cacheWrite: number;
  output: number;
}

export interface ModelPriceRow {
  model: string;
  standard: PriceRates;
  priority: PriceRates;
  flex: PriceRates;
  longContext: { threshold: number; inputMultiplier: number; outputMultiplier: number } | null;
}

export interface PricingCatalogInfo {
  source: "bundled" | "cache" | "remote";
  sha256: string;
  modelCount: number;
  remoteUrl: string;
  /** When the prices in use were downloaded from the price repo. */
  fetchedAt?: string | null;
  lastCheckedAt?: string | null;
  lastUpdatedAt?: string | null;
  lastError?: string | null;
}

export interface PricingView {
  info: PricingCatalogInfo;
  models: ModelPriceRow[];
}

export interface SavedAccount {
  accountId: string;
  email?: string | null;
  authMode?: string | null;
  /** Access-token imports cannot refresh and must be re-imported on expiry. */
  refreshable: boolean;
  /** The saved credentials still look usable. */
  usable: boolean;
  active: boolean;
  addedAt: string;
  lastUsedAt?: string | null;
  /** Installation id of the account's own virtual device. */
  deviceId?: string | null;
  /** The account's outbound line, e.g. "手动代理 · socks5://host:port". */
  network?: string | null;
}
