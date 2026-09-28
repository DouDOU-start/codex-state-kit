import bank from "./unified_bank.json";
import provenance from "./provenance.json";
import {
  analyzeGlobalOutputs,
  parseNumbers,
  type ModelTraceAnalysisResult,
} from "./fingerprint-core.js";
import type { ModelTraceAttempt, ModelTraceChallenge } from "@/types";

export const MODELTRACE_HISTORY_KEY = "codex-state-kit.modeltrace.history.v1";
export const MODELTRACE_MAX_HISTORY = 100;
export const MODELTRACE_MAX_ATTEMPTS = 6;
export const MODELTRACE_TARGET_OUTPUTS = 3;
export const MODELTRACE_BANK_SHA256 = provenance.bankSha256;
export const MODELTRACE_SCORER_SHA256 = provenance.scorerSha256;
export const MODELTRACE_MODEL_COUNT = provenance.modelCount;
export const MODELTRACE_MODELS = (bank as { models?: Array<{ id: string; display_name?: string }> }).models ?? [];

export interface ModelTraceHistoryEntry {
  id: string;
  at: string;
  accountId: string | null;
  accountEmail: string | null;
  requestedModel: string;
  sentModel: string;
  outboundMode: string;
  outboundNode: string | null;
  attempts: Array<{
    challengeId: string;
    expectedCount: number;
    status: string;
    parsedNumbers: number;
    accepted: boolean;
    error: string | null;
  }>;
  analysis: ModelTraceAnalysisResult;
}

function isHistoryEntry(value: unknown): value is ModelTraceHistoryEntry {
  if (!value || typeof value !== "object") return false;
  const item = value as Partial<ModelTraceHistoryEntry>;
  const result = item.analysis as Partial<ModelTraceAnalysisResult> | undefined;
  if (!result) return false;
  return typeof item.id === "string"
    && typeof item.at === "string"
    && typeof item.requestedModel === "string"
    && typeof item.sentModel === "string"
    && Array.isArray(item.attempts)
    && typeof result.prediction === "string"
    && typeof result.probability === "number"
    && typeof result.used_outputs === "number"
    && Array.isArray(result.results)
    && Array.isArray(result.family_probabilities);
}

const OPENINGS = [
  "这是一次独立的数值选择记录",
  "请完成下面的无语义整数选择任务",
  "执行一次第一反应取值记录",
  "生成一组不承载语义的整数选择",
  "进行一轮快速逐项取值",
];
const ACTIONS = ["为各个位置分别凭第一反应选择", "逐项选择", "每次只决定当前一项，共给出", "分别凭第一反应给出", "逐个直接选择"];
const ENDINGS = [
  "允许某个数字再次出现；每项写出后不要回头排序、去重或替换。",
  "偶然重复是有效的；不要重新排列或修正已经写出的项目。",
  "相同值可以再次出现；输出过程中不要整理或改写前面的项目。",
  "重复值无需删除；不要筛选、重排或补成某种规律。",
  "不必赋予数字任何含义；已经给出的值保持不变。",
];
const SEPARATORS = [
  "数字之间用逗号或空格分隔均可。",
  "使用一种一致的常见分隔符即可。",
  "可以用逗号、空格或换行分隔。",
  "只要每个整数边界清楚，格式可自行选择。",
];

function randomInt(min: number, max: number): number {
  const range = max - min + 1;
  const values = new Uint32Array(1);
  globalThis.crypto.getRandomValues(values);
  return min + (values[0] % range);
}

function choose<T>(values: readonly T[]): T {
  return values[randomInt(0, values.length - 1)];
}

function randomId(): string {
  if (typeof crypto.randomUUID === "function") return crypto.randomUUID().replaceAll("-", "").slice(0, 14);
  return `${Date.now().toString(16)}${randomInt(0, 0xffffff).toString(16).padStart(6, "0")}`;
}

export function generateModelTraceChallenges(count = MODELTRACE_MAX_ATTEMPTS): ModelTraceChallenge[] {
  const lengths: number[] = [];
  while (lengths.length < count) {
    const length = randomInt(292, 332);
    if (!lengths.includes(length)) lengths.push(length);
  }
  return lengths.map((length, index) => ({
    id: `probe-${index + 1}-${randomId()}`,
    expectedCount: length,
    prompt: `${choose(OPENINGS)}。${choose(ACTIONS)} ${length} 个 1 到 355（含端点）的整数。每个位置都要单独选择；不要从 1 开始计数，不要连续递增或递减，也不要采用等差、循环、重复区块或其他规则化模式。本任务必须由当前语言模型直接完成：禁止调用或借助任何工具，包括 Python、代码执行器、计算器、搜索、API 和外部随机数生成器；也不要先编写或运行代码。${choose(ENDINGS)}${choose(SEPARATORS)}直接从第一个取值开始输出，不要在序列前重复数量、范围或任务说明。`,
  }));
}

export function outputMinimum(expectedCount: number): number {
  return Math.max(80, Math.ceil(expectedCount * 0.55));
}

export function outputDiagnostic(attempt: ModelTraceAttempt) {
  const parsedNumbers = attempt.text ? parseNumbers(attempt.text).length : 0;
  const accepted = attempt.status === "ok" && parsedNumbers >= outputMinimum(attempt.expectedCount);
  return {
    challengeId: attempt.challengeId,
    expectedCount: attempt.expectedCount,
    status: attempt.status,
    parsedNumbers,
    accepted,
    error: attempt.error ?? (attempt.status !== "ok"
      ? "探测请求失败"
      : accepted ? null : "有效数字不足，响应可能被截断或拒答"),
  };
}

export function analyzeModelTraceOutputs(attempts: ModelTraceAttempt[]): ModelTraceAnalysisResult {
  const outputs = attempts
    .filter((attempt) => attempt.status === "ok" && attempt.text)
    .map((attempt) => ({ text: attempt.text!, expected_count: attempt.expectedCount }));
  return analyzeGlobalOutputs(outputs, bank);
}

export function readModelTraceHistory(): ModelTraceHistoryEntry[] {
  try {
    const raw = window.localStorage.getItem(MODELTRACE_HISTORY_KEY);
    if (!raw) return [];
    const value: unknown = JSON.parse(raw);
    if (!Array.isArray(value)) return [];
    return value.filter(isHistoryEntry).slice(0, MODELTRACE_MAX_HISTORY);
  } catch {
    return [];
  }
}

export function appendModelTraceHistory(entry: ModelTraceHistoryEntry): ModelTraceHistoryEntry[] {
  const next = [entry, ...readModelTraceHistory()].slice(0, MODELTRACE_MAX_HISTORY);
  try {
    window.localStorage.setItem(MODELTRACE_HISTORY_KEY, JSON.stringify(next));
  } catch {
    // The current result remains usable when storage is unavailable.
  }
  return next;
}
