import { useSyncExternalStore } from "react";
import { getLocale, subscribeLocale } from "@/lib/i18n";

/** Subscribe without remounting screens or discarding unsaved form state. */
export function useLocale() {
  return useSyncExternalStore(subscribeLocale, getLocale, getLocale);
}
