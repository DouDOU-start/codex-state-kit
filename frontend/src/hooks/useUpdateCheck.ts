import { t } from "@/lib/i18n";
import { useCallback, useEffect, useRef, useState } from "react";
import { isTauri } from "@/lib/api";
import { checkUpdate, openReleasePage, type UpdateInfo } from "@/lib/update";
import { invoke } from "@tauri-apps/api/core";
import { check as checkInstaller, type Update } from "@tauri-apps/plugin-updater";

export function useUpdateCheck() {
  const [info, setInfo] = useState<UpdateInfo | null>(null);
  const [checking, setChecking] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [dismissed, setDismissed] = useState<string | null>(null);
  const alive = useRef(true);
  const busy = useRef(false);
  const installer = useRef<Update | null>(null);
  const installing = useRef(false);
  const [phase, setPhase] = useState<"idle" | "downloading" | "ready" | "installing">("idle");
  const [progress, setProgress] = useState<number | null>(null);

  const check = useCallback(async (manual = false) => {
    if (!isTauri) {
      if (manual) setMessage(t("浏览器预览不检查更新，请在桌面应用中使用，或直接查看发布页面。"));
      return;
    }
    if (busy.current || installer.current || installing.current) return;
    busy.current = true;
    setChecking(true);
    if (manual) setMessage(null);
    try {
      const result = await checkUpdate();
      if (!alive.current) return;
      setInfo(result);
      if (manual) {
        setDismissed(null);
        setMessage(result.available ? null : result.latestVersion
          ? t("当前 v{0}，暂无更新（最新正式版 v{1}）。", [result.currentVersion, result.latestVersion])
          : t("尚无可用的正式发布版本。"));
      }
    } catch (error) {
      if (manual && alive.current) setMessage(t("检查更新失败：{0}", [String(error)]));
    } finally {
      busy.current = false;
      if (alive.current) setChecking(false);
    }
  }, []);

  useEffect(() => {
    alive.current = true;
    // Development builds and browser previews only check on explicit request.
    if (!isTauri || import.meta.env.DEV) return () => { alive.current = false; };
    void check();
    const timer = window.setInterval(() => void check(), 6 * 60 * 60 * 1000);
    return () => { alive.current = false; window.clearInterval(timer); };
  }, [check]);

  const open = useCallback(async (tag?: string | null) => {
    try { await openReleasePage(tag); }
    catch { if (alive.current) setMessage(t("无法打开浏览器，请访问 github.com/DouDOU-start/codex-state-kit/releases。")); }
  }, []);

  useEffect(() => () => {
    if (!installing.current) void installer.current?.close().catch(() => {});
  }, []);

  const download = useCallback(async () => {
    if (!isTauri || busy.current || installer.current || installing.current) return;
    busy.current = true;
    setPhase("downloading");
    setMessage(null);
    setProgress(null);
    let update: Update | null = null;
    try {
      update = await checkInstaller({ timeout: 15_000 });
      if (!update) throw new Error(t("当前平台暂无可安装的更新，请稍后重试或前往发布页面。"));
      if (update.version !== info?.latestVersion) throw new Error(t("发布版本已变化，请重新检查更新。"));
      let received = 0;
      let total = 0;
      await update.download((event) => {
        if (!alive.current) return;
        if (event.event === "Started") total = event.data.contentLength ?? 0;
        if (event.event === "Progress") {
          received += event.data.chunkLength;
          setProgress(total ? Math.min(100, Math.round(received / total * 100)) : null);
        }
      }, { timeout: 10 * 60 * 1000 });
      // download resolves only after signature verification succeeds.
      if (!alive.current) { await update.close(); return; }
      installer.current = update;
      setPhase("ready");
    } catch (error) {
      await update?.close().catch(() => {});
      if (alive.current) {
        setPhase("idle");
        setMessage(t("自动更新包下载或校验失败：{0} 可前往发布页面手动安装。", [String(error)]));
      }
    } finally { busy.current = false; }
  }, [info?.latestVersion]);

  const install = useCallback(async () => {
    if (!installer.current || installing.current) return;
    installing.current = true;
    setPhase("installing");
    setMessage(null);
    try {
      await invoke("prepare_update");
      await installer.current.install({ restartAfterInstall: true });
      await invoke("restart_after_update");
    } catch (error) {
      let recoveryError = "";
      try { await invoke("resume_after_update_failure"); }
      catch { recoveryError = t(" 服务恢复失败，请重新启动应用。"); }
      await installer.current?.close().catch(() => {});
      installer.current = null;
      installing.current = false;
      if (alive.current) {
        setPhase("idle");
        setMessage(t("安装更新失败：{0}{1}", [String(error), recoveryError]));
      }
    }
  }, []);

  return {
    checking: checking || phase !== "idle", message, check, open, download, install, phase, progress,
    update: info?.available && info.tag !== dismissed ? info : null,
    dismiss: () => { if (phase === "idle") setDismissed(info?.tag ?? null); setMessage(null); },
  };
}
