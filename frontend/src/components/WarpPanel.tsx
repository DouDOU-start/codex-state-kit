import Cloud from "lucide-react/dist/esm/icons/cloud.js";
import { isTauri } from "@/lib/api";
import type { WarpStatus } from "@/types";

export function WarpPanel({ status, probing, delayText, onProbe, onTerms }: { status: WarpStatus; probing: boolean; delayText: string | null; onProbe: () => void; onTerms: () => void }) {
  const ready = status.phase === "connected";
  const failed = status.phase === "error" || status.phase === "reconnecting";
  const label = !isTauri ? "预览" : ready ? "已就绪" : failed ? "重试中" : "连接中";
  const message = !isTauri ? "桌面应用将自动连接，无需额外配置。" : !status.available ? "内核文件缺失，请使用完整安装包。" : ready ? "已自动为 Token 获取提供出站网络。" : failed ? status.error || "网络暂不可用，正在自动重试。" : "正在准备出站网络，请稍候。";
  return (
    <div className="warp-panel">
      <div className="warp-summary">
        <span className="warp-summary__icon"><Cloud size={21} /></span>
        <div><strong>自动管理出站网络</strong><p>{message}</p></div>
        <span className={`runtime-chip ${ready ? "" : "runtime-chip--idle"}`} role="status"><i />{label}</span>
      </div>
      {isTauri && status.error && !ready ? <p className="warp-error">{status.error}</p> : null}
      <div className="latency-row">
        <button type="button" className="token-fetch-toggle" disabled={probing || (isTauri && !ready)} onClick={onProbe}>
          {probing ? "测试中" : "测延迟"}
        </button>
        {delayText ? <span className={delayText.endsWith("ms") ? "latency-row__ok" : "latency-row__bad"}>{delayText}</span> : null}
      </div>
      <button className="text-button warp-terms" type="button" onClick={onTerms}>Cloudflare 服务条款 ↗</button>
    </div>
  );
}
