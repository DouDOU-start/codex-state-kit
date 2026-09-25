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
export function t(source: string, values: readonly unknown[] = []): string {
  const message = locale === "ru" && Object.hasOwn(ru, source) ? ru[source as keyof typeof ru] : source;
  return message.replace(/\{(\d+)\}/g, (placeholder, index: string) =>
    Number(index) < values.length ? String(values[Number(index)]) : placeholder,
  );
}

if (typeof document !== "undefined") document.documentElement.lang = locale;
