import { t, setLocale, type Locale } from "@/lib/i18n";
import { useLocale } from "@/hooks/useLocale";
import Minus from "lucide-react/dist/esm/icons/minus.js";
import Square from "lucide-react/dist/esm/icons/square.js";
import X from "lucide-react/dist/esm/icons/x.js";
import Github from "lucide-react/dist/esm/icons/github.js";
import ExternalLink from "lucide-react/dist/esm/icons/external-link.js";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useEffect, type PropsWithChildren } from "react";
import { invoke } from "@tauri-apps/api/core";
import { GITHUB_REPO_URL, isTauri, openGithubRepo } from "@/lib/api";
import { version } from "../../../package.json";
import { Logo } from "./Logo";
import { useUpdateCheck } from "@/hooks/useUpdateCheck";
import { useNotice, useNotify } from "@/components/Notifier";
import { Select } from "./Select";

async function windowAction(action: "minimize" | "maximize" | "close") {
  if (!isTauri) return;
  const window = getCurrentWindow();
  if (action === "minimize") await window.minimize();
  if (action === "maximize") await window.toggleMaximize();
  // Keep the proxy and tray process alive when the title-bar X is clicked.
  // The tray menu remains the explicit way to exit the application.
  if (action === "close") await window.hide();
}

export function AppShell({ children }: PropsWithChildren) {
  const locale = useLocale();
  const { notify } = useNotify();
  useEffect(() => {
    if (!isTauri) return;
    void invoke("set_ui_language", { language: locale }).catch((error: unknown) => {
      notify({ kind: "error", title: t("托盘语言更新失败"), message: String(error) });
    });
  }, [locale, notify]);
  const updates = useUpdateCheck();
  const update = updates.update;
  useNotice(
    "update",
    update ? `${update.tag}:${updates.phase}:${updates.progress ?? ""}` : null,
    () => ({
      kind: "info",
      title: updates.phase === "ready" ? t("更新已就绪") : updates.phase === "installing" ? t("正在安装更新") : updates.phase === "downloading" ? t("正在下载更新") : t("发现新版本"),
      message: updates.phase === "ready"
        ? t("v{0} 已下载并通过签名校验。安装将关闭应用，请先结束当前会话。", [update?.latestVersion])
        : updates.phase === "installing"
          ? t("正在恢复路由、停止订阅内核并等待在途请求结束，请勿关闭应用…")
          : updates.phase === "downloading"
            ? t("正在下载 v{0}{1}，下载期间可继续使用。", [update?.latestVersion, updates.progress === null ? "" : ` · ${updates.progress}%`])
            : t("v{0} 已发布（当前 v{1}）。", [update?.latestVersion, update?.currentVersion]),
      sticky: true,
      actions: [
        ...(updates.phase === "idle" ? [{ label: t("下载更新"), primary: true, onClick: () => void updates.download() }] : []),
        ...(updates.phase === "ready" ? [{ label: t("确认安装并重启"), primary: true, onClick: () => void updates.install() }] : []),
        { label: <>{t("前往下载")} <ExternalLink size={11} /></>, onClick: () => void updates.open(update?.tag) },
      ],
      onClose: updates.phase === "idle" ? updates.dismiss : undefined,
    }),
  );
  useNotice("update-message", updates.message ?? null, () => ({
    kind: "info",
    message: updates.message,
    sticky: true,
    actions: [{ label: t("发布页面"), onClick: () => void updates.open() }],
    onClose: updates.dismiss,
  }));
  const versionLabel = import.meta.env.DEV || !isTauri ? "dev" : `v${version}`;
  return (
    <div className="app-shell">
      <header className="titlebar" data-tauri-drag-region>
        <div className="titlebar__identity" data-tauri-drag-region>
          <Logo />
          <span className="app-version" data-tauri-drag-region>{versionLabel}</span>
        </div>
        <div className="titlebar__actions">
          <Select
            className="language-select"
            variant="compact"
            ariaLabel={t("界面语言")}
            value={locale}
            options={[{ value: "zh-CN", label: "简体中文" }, { value: "ru", label: "Русский" }]}
            onChange={(value) => setLocale(value as Locale)}
            disabled={updates.phase === "installing"}
          />
          <button className="update-check" type="button" disabled={updates.checking} onClick={() => void updates.check(true)}>
            {updates.phase === "installing" ? t("安装中…") : updates.phase === "downloading" ? t("下载中…") : updates.checking && updates.phase !== "ready" ? t("检查中…") : t("检查更新")}
          </button>
          <a className="repo-link" href={GITHUB_REPO_URL} target="_blank" rel="noopener noreferrer"
            aria-label={t("在浏览器打开 GitHub 仓库 DouDOU-start/codex-state-kit")}
            title="DouDOU-start/codex-state-kit"
            onClick={(event) => {
              if (!isTauri) return;
              event.preventDefault();
              void openGithubRepo().catch(() => notify({ kind: "error", message: t("无法打开浏览器，请访问 github.com/DouDOU-start/codex-state-kit") }));
            }}>
            <Github size={15} aria-hidden="true" /><span>GitHub</span><ExternalLink size={11} aria-hidden="true" />
          </a>
        <div className="window-controls">
          <button type="button" aria-label={t("最小化")} onClick={() => void windowAction("minimize")}>
            <Minus size={17} />
          </button>
          <button type="button" aria-label={t("最大化")} onClick={() => void windowAction("maximize")}>
            <Square size={13} />
          </button>
          <button className="window-controls__close" type="button" aria-label={t("隐藏到后台")} title={t("隐藏到后台")} onClick={() => void windowAction("close")}>
            <X size={17} />
          </button>
        </div>
        </div>
      </header>
      <main className="app-content">
        <div style={{ display: "contents" }} ref={(node) => { if (node) node.inert = updates.phase === "installing"; }}>
          {children}
        </div>
      </main>
    </div>
  );
}
