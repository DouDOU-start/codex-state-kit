import Gauge from "lucide-react/dist/esm/icons/gauge.js";
import LoaderCircle from "lucide-react/dist/esm/icons/loader-circle.js";
import type { LatencySample } from "@/types";

interface LatencyProbeProps {
  probing: boolean;
  disabled?: boolean;
  /** The last result for this line or node, if any. */
  sample?: LatencySample | null;
  onProbe: () => void;
}

type Grade = "fast" | "mid" | "slow" | "bad";

function grade(sample: LatencySample): Grade {
  if (sample.delayMs == null) return "bad";
  if (sample.delayMs < 300) return "fast";
  return sample.delayMs < 800 ? "mid" : "slow";
}

/**
 * Sits next to the input it tests, at the same height. Shows the last
 * result in place of its label; clicking again re-tests.
 */
export function LatencyProbe({ probing, disabled, sample, onProbe }: LatencyProbeProps) {
  const result = sample && !probing ? sample : null;
  const failed = result ? result.delayMs == null : false;
  const label = probing
    ? "测试中"
    : result
      ? result.delayMs != null ? `${result.delayMs} ms` : /超时|timeout|timed out/i.test(result.error ?? "") || !result.error ? "超时" : "失败"
      : "测延迟";
  return (
    <button
      type="button"
      className="latency-probe"
      data-grade={result ? grade(result) : undefined}
      disabled={disabled || probing}
      aria-label={result ? `延迟 ${label}，点击重新测试` : "测试延迟"}
      title={failed && result?.error ? result.error : result ? "点击重新测试" : undefined}
      onClick={onProbe}
    >
      {probing ? <LoaderCircle size={14} className="is-spinning" /> : result ? <i aria-hidden="true" /> : <Gauge size={14} />}
      <span>{label}</span>
    </button>
  );
}
