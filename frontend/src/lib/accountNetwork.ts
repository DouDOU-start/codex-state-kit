import { t } from "./i18n";

/** Translate only the backend's fixed labels, never node names or proxy URLs. */
export function accountNetwork(network: string): string {
  const separator = network.indexOf(" · ");
  if (separator < 0) return network;
  const mode = network.slice(0, separator);
  if (mode !== "手动代理" && mode !== "订阅节点") return network;
  const detail = network.slice(separator + 3);
  const label = mode === "手动代理" && (detail === "未配置" || detail === "已配置")
    ? t(detail)
    : detail;
  return `${t(mode)} · ${label}`;
}
