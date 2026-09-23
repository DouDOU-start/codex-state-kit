import { useCallback, useEffect, useMemo, useState } from "react";
import BadgeDollarSign from "lucide-react/dist/esm/icons/badge-dollar-sign.js";
import RefreshCw from "lucide-react/dist/esm/icons/refresh-cw.js";
import { getPricing, isTauri, syncPricing } from "@/lib/api";
import type { ModelPriceRow, PricingCatalogInfo, PricingView } from "@/types";
import { useNotify } from "@/components/Notifier";

interface PricingPanelProps {
  /** 所在 tab 是否可见；切到该 tab 时重新读取价格表。 */
  active: boolean;
}

/** Codex 实际会用到的模型；其余 OpenAI 模型勾选「全部模型」后显示。 */
const CODEX_MODEL = /^(gpt-5|gpt-6|codex)/;

const SOURCE_LABEL: Record<PricingCatalogInfo["source"], string> = {
  bundled: "内置价格表",
  cache: "本地缓存",
  remote: "远程同步",
};

function perMillion(usdPerToken: number): string {
  const value = usdPerToken * 1_000_000;
  if (!Number.isFinite(value)) return "—";
  return `$${Number(value.toPrecision(6)).toString()}`;
}

function formatTime(value?: string | null): string {
  if (!value) return "—";
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? value : date.toLocaleString("zh-CN", { hour12: false });
}

function longContextLabel(row: ModelPriceRow): string {
  const lc = row.longContext;
  if (!lc) return "—";
  return `>${Math.round(lc.threshold / 1000)}K 输入 ×${lc.inputMultiplier} · 输出 ×${lc.outputMultiplier}`;
}

export function PricingPanel({ active }: PricingPanelProps) {
  const [view, setView] = useState<PricingView | null>(null);
  const [syncing, setSyncing] = useState(false);
  const { notify } = useNotify();
  const [query, setQuery] = useState("");
  const [showAll, setShowAll] = useState(false);

  const load = useCallback(async () => {
    try {
      setView(await getPricing());
    } catch (cause) {
      notify({ kind: "error", title: "读取模型价格失败", message: cause instanceof Error ? cause.message : String(cause) });
    }
  }, [notify]);

  useEffect(() => {
    if (active) void load();
  }, [active, load]);

  const sync = async () => {
    setSyncing(true);
    try {
      setView(await syncPricing());
    } catch (cause) {
      notify({ kind: "error", title: "同步模型价格失败", message: cause instanceof Error ? cause.message : String(cause) });
      void load();
    } finally {
      setSyncing(false);
    }
  };

  const rows = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return (view?.models ?? []).filter((row) =>
      (showAll || CODEX_MODEL.test(row.model)) && (!needle || row.model.includes(needle)),
    );
  }, [view, query, showAll]);

  const info = view?.info;

  return (
    <div className="usage-records">
      <header className="usage-records__header">
        <div className="section-heading">
          <span className="section-icon"><BadgeDollarSign size={19} /></span>
          <div>
            <h2>模型价格</h2>
            <p>
              {info ? `${SOURCE_LABEL[info.source]} · ${info.modelCount} 个模型` : "读取中…"}
              {info?.sha256 ? ` · ${info.sha256.slice(0, 12)}` : ""}
              {!isTauri ? " · 浏览器示例" : ""}
            </p>
          </div>
        </div>
        <div className="usage-records__actions">
          <button className="billing-panel__refresh" type="button" disabled={syncing} onClick={() => void sync()}>
            <RefreshCw size={13} className={syncing ? "is-spinning" : undefined} />
            {syncing ? "同步中" : "立即同步"}
          </button>
        </div>
      </header>
      <div className="pricing-sync">
        <span>每 10 分钟比对 sub2api 价格仓库的 sha256，有变化自动下载并校验</span>
        <span>上次检查 {formatTime(info?.lastCheckedAt)}</span>
        <span>上次更新 {formatTime(info?.lastUpdatedAt)}</span>
        {info?.lastError ? <span>最近一次检查失败：{info.lastError}</span> : null}
      </div>
      <div className="usage-record-filter">
        <label>
          搜索
          <input
            className="pricing-search"
            type="search"
            spellCheck={false}
            value={query}
            placeholder="例如 codex"
            onChange={(event) => setQuery(event.target.value)}
          />
        </label>
        <label className="pricing-toggle">
          <input type="checkbox" checked={showAll} onChange={(event) => setShowAll(event.target.checked)} />
          显示全部 OpenAI 模型
        </label>
      </div>
      <div className="usage-records__table">
        {rows.length ? (
          <table className="usage-table pricing-table">
            <thead>
              <tr>
                <th>模型</th>
                <th>输入</th>
                <th>缓存读</th>
                <th>缓存写</th>
                <th>输出</th>
                <th>Priority 输入 / 输出</th>
                <th>长上下文</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr key={row.model}>
                  <td className="usage-table__model"><strong>{row.model}</strong></td>
                  <td>{perMillion(row.standard.input)}</td>
                  <td>{perMillion(row.standard.cacheRead)}</td>
                  <td>{perMillion(row.standard.cacheWrite)}</td>
                  <td>{perMillion(row.standard.output)}</td>
                  <td>{perMillion(row.priority.input)} / {perMillion(row.priority.output)}</td>
                  <td className="pricing-table__long">{longContextLabel(row)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : (
          <div className="usage-records__empty">
            <BadgeDollarSign size={24} strokeWidth={1.5} />
            <strong>{view ? "没有匹配的模型" : "正在读取价格表"}</strong>
          </div>
        )}
      </div>
      <p className="pricing-note">
        单位：美元 / 百万 tokens。费用 = 非缓存输入 × 输入价 + 缓存读 × 缓存读价 + 缓存写 × 缓存写价 + 输出（含推理）× 输出价；
        Priority 档按表中价格，Flex 档为标准价一半，输入超过长上下文阈值时按倍率加价。
      </p>
    </div>
  );
}
