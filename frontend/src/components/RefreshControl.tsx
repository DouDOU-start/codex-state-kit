import { t } from "@/lib/i18n";
import { useLocale } from "@/hooks/useLocale";
import RefreshCw from "lucide-react/dist/esm/icons/refresh-cw.js";
import { Select } from "@/components/Select";

export const REFRESH_OPTIONS = [
  { value: "0", label: "不自动刷新" },
  { value: "1000", label: "每 1 秒" },
  { value: "3000", label: "每 3 秒" },
  { value: "5000", label: "每 5 秒" },
  { value: "10000", label: "每 10 秒" },
  { value: "30000", label: "每 30 秒" },
];

interface RefreshControlProps {
  loading: boolean;
  onRefresh: () => void;
  intervalMs: number;
  onIntervalChange: (intervalMs: number) => void;
}

/** A manual refresh button next to the auto-refresh interval. */
export function RefreshControl({ loading, onRefresh, intervalMs, onIntervalChange }: RefreshControlProps) {
  useLocale();
  return (
    <div className="refresh-control">
      <button className="billing-panel__refresh" type="button" disabled={loading} onClick={onRefresh}>
        <RefreshCw size={13} className={loading ? "is-spinning" : undefined} />
        {loading ? t("读取中") : t("刷新")}
      </button>
      <Select
        variant="compact"
        ariaLabel={t("自动刷新")}
        value={String(intervalMs)}
        options={REFRESH_OPTIONS.map((option) => ({ ...option, label: t(option.label) }))}
        onChange={(value) => onIntervalChange(Number(value))}
      />
    </div>
  );
}
