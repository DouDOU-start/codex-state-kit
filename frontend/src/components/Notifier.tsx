import { createContext, useCallback, useContext, useEffect, useMemo, useRef, useState, type PropsWithChildren, type ReactNode } from "react";
import CircleAlert from "lucide-react/dist/esm/icons/circle-alert.js";
import CircleCheck from "lucide-react/dist/esm/icons/circle-check.js";
import Info from "lucide-react/dist/esm/icons/info.js";
import TriangleAlert from "lucide-react/dist/esm/icons/triangle-alert.js";
import X from "lucide-react/dist/esm/icons/x.js";

export type NoticeKind = "ok" | "error" | "warn" | "info";

export interface NoticeAction {
  label: ReactNode;
  onClick: () => void;
  primary?: boolean;
}

export interface NoticeInput {
  /** Reusing an id replaces that notice in place instead of stacking. */
  id?: string;
  kind: NoticeKind;
  title?: string;
  message: ReactNode;
  actions?: NoticeAction[];
  /** Stays until closed. Errors and warnings are sticky by default. */
  sticky?: boolean;
  /** Called when the user closes the notice. */
  onClose?: () => void;
}

interface Notice extends NoticeInput {
  id: string;
}

export interface ConfirmOptions {
  title: string;
  message: ReactNode;
  confirmText?: string;
  cancelText?: string;
  /** Red confirm button for destructive actions. */
  danger?: boolean;
}

interface NotifyApi {
  notify: (input: NoticeInput) => string;
  dismiss: (id: string) => void;
  confirm: (options: ConfirmOptions) => Promise<boolean>;
}

const NotifyContext = createContext<NotifyApi | null>(null);
const AUTO_DISMISS_MS = 4500;

const ICONS: Record<NoticeKind, typeof Info> = {
  ok: CircleCheck,
  error: CircleAlert,
  warn: TriangleAlert,
  info: Info,
};

let nextId = 0;

/** Themed notices (top-right) and confirm dialogs, replacing banners and window.confirm. */
export function NotifyProvider({ children }: PropsWithChildren) {
  const [notices, setNotices] = useState<Notice[]>([]);
  const timers = useRef(new Map<string, number>());
  const [pendingConfirm, setPendingConfirm] = useState<(ConfirmOptions & { resolve: (ok: boolean) => void }) | null>(null);
  const dialogRef = useRef<HTMLDialogElement>(null);

  const dismiss = useCallback((id: string) => {
    const timer = timers.current.get(id);
    if (timer !== undefined) window.clearTimeout(timer);
    timers.current.delete(id);
    setNotices((current) => current.filter((notice) => notice.id !== id));
  }, []);

  const notify = useCallback((input: NoticeInput) => {
    const id = input.id ?? `notice-${++nextId}`;
    const notice: Notice = { ...input, id };
    setNotices((current) => {
      const index = current.findIndex((item) => item.id === id);
      if (index < 0) return [...current, notice];
      const next = current.slice();
      next[index] = notice;
      return next;
    });
    const previous = timers.current.get(id);
    if (previous !== undefined) window.clearTimeout(previous);
    timers.current.delete(id);
    const sticky = input.sticky ?? (input.kind === "error" || input.kind === "warn" || Boolean(input.actions?.length));
    if (!sticky) timers.current.set(id, window.setTimeout(() => dismiss(id), AUTO_DISMISS_MS));
    return id;
  }, [dismiss]);

  const confirm = useCallback((options: ConfirmOptions) => new Promise<boolean>((resolve) => {
    setPendingConfirm({ ...options, resolve });
  }), []);

  useEffect(() => {
    const dialog = dialogRef.current;
    if (!dialog) return;
    if (pendingConfirm && !dialog.open) dialog.showModal();
    if (!pendingConfirm && dialog.open) dialog.close();
  }, [pendingConfirm]);

  useEffect(() => () => timers.current.forEach((timer) => window.clearTimeout(timer)), []);

  const settle = (ok: boolean) => {
    pendingConfirm?.resolve(ok);
    setPendingConfirm(null);
  };

  const api = useMemo(() => ({ notify, dismiss, confirm }), [notify, dismiss, confirm]);

  return (
    <NotifyContext.Provider value={api}>
      {children}
      <div className="notices" aria-live="polite">
        {notices.map((notice) => {
          const Icon = ICONS[notice.kind];
          return (
            <div key={notice.id} className={`notice notice--${notice.kind}`} role={notice.kind === "error" ? "alert" : "status"}>
              <Icon size={16} className="notice__icon" aria-hidden="true" />
              <div className="notice__body">
                {notice.title ? <strong>{notice.title}</strong> : null}
                <div className="notice__message">{notice.message}</div>
                {notice.actions?.length ? (
                  <div className="notice__actions">
                    {notice.actions.map((action, index) => (
                      <button key={index} type="button" className={action.primary ? "notice__action notice__action--primary" : "notice__action"} onClick={action.onClick}>
                        {action.label}
                      </button>
                    ))}
                  </div>
                ) : null}
              </div>
              <button
                type="button"
                className="notice__close"
                aria-label="关闭提醒"
                onClick={() => {
                  notice.onClose?.();
                  dismiss(notice.id);
                }}
              >
                <X size={14} />
              </button>
            </div>
          );
        })}
      </div>
      <dialog
        ref={dialogRef}
        className="modal modal--confirm"
        aria-labelledby="confirm-title"
        onCancel={(event) => {
          event.preventDefault();
          settle(false);
        }}
        onClick={(event) => {
          if (event.target === dialogRef.current) settle(false);
        }}
      >
        {pendingConfirm ? (
          <div className="modal__surface">
            <div className="confirm">
              <span className={pendingConfirm.danger ? "confirm__icon confirm__icon--danger" : "confirm__icon"} aria-hidden="true">
                {pendingConfirm.danger ? <TriangleAlert size={18} /> : <Info size={18} />}
              </span>
              <div>
                <h2 id="confirm-title">{pendingConfirm.title}</h2>
                <div className="confirm__message">{pendingConfirm.message}</div>
              </div>
            </div>
            <div className="confirm__actions">
              <button type="button" className="button button--ghost" onClick={() => settle(false)}>
                {pendingConfirm.cancelText ?? "取消"}
              </button>
              <button
                type="button"
                autoFocus
                className={pendingConfirm.danger ? "button button--danger" : "button button--primary"}
                onClick={() => settle(true)}
              >
                {pendingConfirm.confirmText ?? "确定"}
              </button>
            </div>
          </div>
        ) : null}
      </dialog>
    </NotifyContext.Provider>
  );
}

export function useNotify(): NotifyApi {
  const api = useContext(NotifyContext);
  if (!api) throw new Error("useNotify must be used inside NotifyProvider");
  return api;
}

/**
 * Shows a notice while `key` is non-null and removes it when `key` becomes
 * null. The notice is rebuilt only when `key` changes, so a notice the user
 * closed stays closed until the underlying state changes.
 */
export function useNotice(id: string, key: string | null, build: () => Omit<NoticeInput, "id">) {
  const { notify, dismiss } = useNotify();
  const buildRef = useRef(build);
  buildRef.current = build;
  useEffect(() => {
    if (key === null) {
      dismiss(id);
      return;
    }
    notify({ ...buildRef.current(), id });
  }, [id, key, notify, dismiss]);
  useEffect(() => () => dismiss(id), [id, dismiss]);
}
