import { ru } from "../locales/ru";

export type Locale = "zh-CN" | "ru";
export const LOCALE_STORAGE_KEY = "codex-state-kit.language";
const listeners = new Set<() => void>();

export function readLocale(): Locale {
  try {
    return window.localStorage.getItem(LOCALE_STORAGE_KEY) === "ru" ? "ru" : "zh-CN";
  } catch {
    // Private/embedded browsers may deny storage; retain the upstream default.
    return "zh-CN";
  }
}

let locale = readLocale();

export function getLocale(): Locale {
  return locale;
}

export function formatNumber(value: number, options: Intl.NumberFormatOptions = {}): string {
  return new Intl.NumberFormat(getLocale(), options).format(value);
}

/** Format large token counts with a compact unit for dashboard metrics. */
export function formatCompactTokens(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "—";
  const absolute = Math.abs(value);
  if (absolute >= 1_000_000_000) {
    return `${formatNumber(value / 1_000_000_000, { minimumFractionDigits: 2, maximumFractionDigits: 2, useGrouping: false })}B`;
  }
  if (absolute >= 1_000_000) {
    return `${formatNumber(value / 1_000_000, { minimumFractionDigits: 2, maximumFractionDigits: 2, useGrouping: false })}M`;
  }
  if (absolute >= 1_000) {
    return `${formatNumber(value / 1_000, { minimumFractionDigits: 1, maximumFractionDigits: 1, useGrouping: false })}K`;
  }
  return formatNumber(value);
}

export function subscribeLocale(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

export function setLocale(next: Locale): void {
  if (next !== "zh-CN" && next !== "ru") return;
  try {
    window.localStorage.setItem(LOCALE_STORAGE_KEY, next);
  } catch {
    // Switching still works for this session when persistence is unavailable.
  }
  locale = next;
  if (typeof document !== "undefined") document.documentElement.lang = next;
  listeners.forEach((listener) => listener());
}

/** Chinese source messages remain the fallback, including in future locales. */
export function t(source: string | null | undefined, values: readonly unknown[] = []): string {
  const fallback = source == null ? "" : String(source);
  const message = locale === "ru" && Object.hasOwn(ru, fallback) ? ru[fallback as keyof typeof ru] : fallback;
  return message.replace(/\{(\d+)\}/g, (placeholder, index: string) =>
    Number(index) < values.length ? String(values[Number(index)]) : placeholder,
  );
}

if (typeof document !== "undefined") document.documentElement.lang = locale;
