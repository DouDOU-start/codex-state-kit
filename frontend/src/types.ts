export interface LogEntry {
  id: number;
  ts: string;
  method: string;
  path: string;
  status: number;
  ms: number;
  flow: string;
  transport: string;
  targetOrigin: string;
  finalOrigin?: string | null;
  routeKind: string;
  proxyEndpoint?: string | null;
  peerAddr?: string | null;
  httpVersion?: string | null;
  model?: string | null;
  contentEncoding: string;
  bodyBytes: number;
  turnStateAction: string;
  turnStateLen?: number | null;
  returnedTurnStateLen?: number | null;
  errorKind?: string | null;
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
}

export interface TurnStateView {
  status: "idle" | "active" | "partial" | "empty" | string;
  ageSecs?: number | null;
  len?: number | null;
  source?: string | null;
  capturedAt?: string | null;
  models?: ModelTokenView[];
  boundTokenLen?: number;
}

export interface Status {
  proxyListen: string;
  upstream: string;
  codexHome: string;
  proxyOk: boolean;
  attached: boolean;
  proxyError?: string | null;
  attachError?: string | null;
  outboundProxy: string;
  upstreamProxy: string;
  outboundMode: OutboundMode;
  warpHttp2: boolean;
  warp: WarpStatus;
  fetchError?: string | null;
  fetchOkAt?: string | null;
  turnState: TurnStateView;
  degraded: boolean;
  degradedAt?: string | null;
  logs: LogEntry[];
}

export interface SettingsPatch {
  proxyListen: string;
  upstream: string;
  codexHome: string;
  outboundProxy: string;
  upstreamProxy: string;
  outboundMode: OutboundMode;
  warpHttp2: boolean;
}

export type OutboundMode = "manual" | "warp";

export interface WarpStatus {
  available: boolean;
  registered: boolean;
  phase: "stopped" | "starting" | "registering" | "connecting" | "connected" | "reconnecting" | "error";
  proxyUrl: string | null;
  exitIp: string | null;
  country: string | null;
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
}

export type LoginMethod = "device" | "browser";

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
