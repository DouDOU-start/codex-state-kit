import { t } from "@/lib/i18n";
import { useLocale } from "@/hooks/useLocale";
import { useEffect, useRef, useState } from "react";
import Copy from "lucide-react/dist/esm/icons/copy.js";
import ExternalLink from "lucide-react/dist/esm/icons/external-link.js";
import LogIn from "lucide-react/dist/esm/icons/log-in.js";
import Radio from "lucide-react/dist/esm/icons/radio.js";
import Shield from "lucide-react/dist/esm/icons/shield.js";
import KeyRound from "lucide-react/dist/esm/icons/key-round.js";
import UserPlus from "lucide-react/dist/esm/icons/user-plus.js";
import X from "lucide-react/dist/esm/icons/x.js";
import type { useCodexStateKit } from "@/hooks/useCodexStateKit";
import type { LoginMode, SavedAccount } from "@/types";

interface AddAccountDialogProps {
  open: boolean;
  /** A saved account to sign in again; null adds a new account. */
  target: SavedAccount | null;
  onClose: () => void;
  fwd: ReturnType<typeof useCodexStateKit>;
  codexHome: string;
}

const METHODS: { mode: LoginMode; label: string; Icon: typeof Copy }[] = [
  { mode: "browser", label: "浏览器回调", Icon: ExternalLink },
  { mode: "device", label: "授权码登录", Icon: Copy },
  { mode: "refresh", label: "Refresh Token", Icon: Radio },
  { mode: "access", label: "Access Token", Icon: Shield },
];

/**
 * Signs in a new ChatGPT account, or signs a saved one in again when its
 * authorization expired. Closes itself once the login succeeds.
 */
export function AddAccountDialog({ open, target, onClose, fwd, codexHome }: AddAccountDialogProps) {
  useLocale();
  const dialogRef = useRef<HTMLDialogElement>(null);
  const [mode, setMode] = useState<LoginMode>("browser");
  const [refreshToken, setRefreshToken] = useState("");
  const [accessToken, setAccessToken] = useState("");
  const pending = fwd.device;
  const busy = fwd.busy !== null;
  const wasPending = useRef(false);
  const targetId = target?.accountId;
  const targetName = target ? target.email || target.accountId : "";

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (open && !dialog.open) {
      // An Access Token account can only be renewed with a new token.
      setMode(target?.authMode === "chatgptAuthTokens" ? "access" : "browser");
      dialog.showModal();
    }
    if (!open && dialog.open) dialog.close();
  }, [open, target]);

  // A browser or device-code login finished: close when it succeeded.
  useEffect(() => {
    if (!open) {
      wasPending.current = false;
      return;
    }
    if (pending) {
      wasPending.current = true;
    } else if (wasPending.current) {
      wasPending.current = false;
      if (fwd.banner && fwd.banner.kind !== "error") onClose();
    }
  }, [open, pending, fwd.banner, onClose]);

  const close = () => {
    if (pending) void fwd.cancelLogin();
    onClose();
  };

  const copyCode = async () => {
    if (!pending?.userCode) return;
    try {
      await navigator.clipboard.writeText(pending.userCode);
    } catch {
      // ignore
    }
  };

  const submitRefresh = async () => {
    const token = refreshToken.trim();
    if (!token) return;
    if (await fwd.importRefreshLogin(codexHome, token, targetId)) {
      setRefreshToken("");
      onClose();
    }
  };

  const submitAccess = async () => {
    const token = accessToken.trim();
    if (!token) return;
    if (await fwd.importAccessLogin(codexHome, token, targetId)) {
      setAccessToken("");
      onClose();
    }
  };

  return (
    <dialog
      ref={dialogRef}
      className="modal"
      aria-labelledby="add-account-title"
      onCancel={(event) => {
        event.preventDefault();
        close();
      }}
      onClick={(event) => {
        if (event.target === dialogRef.current) close();
      }}
    >
      <div className="modal__surface">
        <header className="modal__header">
          <div className="section-heading">
            <span className="section-icon">{target ? <KeyRound size={19} /> : <UserPlus size={19} />}</span>
            <div>
              <h2 id="add-account-title">{target ? t("重新授权") : t("添加账号")}</h2>
              <p>{target ? t("用 {0} 重新登录，更新保存的凭据并切换为当前账号", [targetName]) : t("登录后自动加入账号列表并切换为当前账号")}</p>
            </div>
          </div>
          <button className="modal__close" type="button" aria-label={t("关闭")} onClick={close}>
            <X size={17} />
          </button>
        </header>
        <div className="modal__body">
          <div className="proxy-mode login-methods" role="group" aria-label={t("登录方式")}>
            {METHODS.map(({ mode: item, label, Icon }) => (
              <button
                key={item}
                type="button"
                aria-pressed={mode === item}
                disabled={busy || Boolean(pending)}
                onClick={() => setMode(item)}
              >
                <Icon size={14} />
                {t(label)}
              </button>
            ))}
          </div>
          <div className="login-box">
            {pending ? (
              <div className="login-pending">
                <p>{pending.method === "browser" ? t("请在浏览器完成授权，登录结果将自动同步。") : t("在浏览器打开验证页并输入代码")}</p>
                {pending.method === "device" ? <div className="user-code">{pending.userCode}</div> : null}
                <div className="panel__actions">
                  {pending.method === "device" ? (
                    <button className="button button--secondary" type="button" onClick={() => void copyCode()}>
                      <Copy size={14} />
                      {t("复制")} </button>
                  ) : null}
                  <button className="button button--secondary" type="button" onClick={() => void fwd.openLoginPage()}>
                    <ExternalLink size={14} />
                    {t("打开页面")} </button>
                  <button className="button button--ghost" type="button" onClick={() => void fwd.cancelLogin()}>
                    {t("取消")} </button>
                </div>
              </div>
            ) : mode === "refresh" ? (
              <div className="credential-import">
                <label className="field">
                  <span>Refresh Token</span>
                  <input
                    type="password"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={busy}
                    value={refreshToken}
                    placeholder={t("粘贴 Refresh Token")}
                    onChange={(event) => setRefreshToken(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") void submitRefresh();
                    }}
                  />
                </label>
                <p>{t("会先向官方授权服务换取新凭据，并保存服务端返回的轮换 Refresh Token。")}</p>
                <div className="panel__actions">
                  <button className="button button--primary" type="button" disabled={busy || !refreshToken.trim()} onClick={() => void submitRefresh()}>
                    {fwd.busy === "login" ? <span className="spinner" /> : <LogIn size={14} />}
                    {t("导入并登录")} </button>
                </div>
              </div>
            ) : mode === "access" ? (
              <div className="credential-import">
                <label className="field">
                  <span>Access Token</span>
                  <input
                    type="password"
                    spellCheck={false}
                    autoComplete="off"
                    disabled={busy}
                    value={accessToken}
                    placeholder={t("粘贴 Codex Access Token（JWT）")}
                    onChange={(event) => setAccessToken(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") void submitAccess();
                    }}
                  />
                </label>
                <p className="credential-warning">{t("Access Token 不可自动刷新；过期后需要重新导入。")}</p>
                <div className="panel__actions">
                  <button className="button button--primary" type="button" disabled={busy || !accessToken.trim()} onClick={() => void submitAccess()}>
                    {fwd.busy === "login" ? <span className="spinner" /> : <LogIn size={14} />}
                    {t("导入并登录")} </button>
                </div>
              </div>
            ) : (
              <div className="login-current">
                <span>{mode === "browser" ? t("在浏览器中登录 ChatGPT，完成后自动返回。") : t("获取授权码后，在浏览器验证页输入即可。")}</span>
                <button className="button button--primary" type="button" disabled={busy} onClick={() => void fwd.startLogin(codexHome, mode, targetId)}>
                  {fwd.busy === "login" ? <span className="spinner" /> : <LogIn size={14} />}
                  {mode === "browser" ? t("打开浏览器登录") : t("获取授权码")}
                </button>
              </div>
            )}
          </div>
          <p className="panel__hint">
            {target
              ? t("账号保留原来的虚拟设备和出站线路；登录请求走它自己的手动代理，订阅节点账号走当前线路。请登录同一个账号，否则会作为另一个账号保存。")
              : t("新账号会分配独立的虚拟设备，并沿用当前出站线路（登录请求也走这条线路）。已添加过的账号再次登录会直接更新它的凭据。")}
            {t("凭据提交后不回显、不写日志；关闭 Kit 时还原原来的官方账号、路由和本地模型配置。")} </p>
        </div>
      </div>
    </dialog>
  );
}
