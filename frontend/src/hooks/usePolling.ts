import { useEffect, useRef } from "react";

/**
 * Calls `tick` every `intervalMs` while `enabled`, skipping ticks while the
 * window is hidden. A tick never overlaps the previous one; 0 disables it.
 */
export function usePolling(tick: () => Promise<void> | void, intervalMs: number, enabled: boolean) {
  const tickRef = useRef(tick);
  tickRef.current = tick;
  useEffect(() => {
    if (!enabled || intervalMs <= 0) return;
    let stopped = false;
    let timer: number | undefined;
    const run = async () => {
      if (stopped) return;
      if (document.visibilityState === "visible") {
        try {
          await tickRef.current();
        } catch {
          // the tick reports its own errors
        }
      }
      if (!stopped) timer = window.setTimeout(run, intervalMs);
    };
    timer = window.setTimeout(run, intervalMs);
    return () => {
      stopped = true;
      window.clearTimeout(timer);
    };
  }, [enabled, intervalMs]);
}
