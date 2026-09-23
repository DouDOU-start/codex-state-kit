import { useState } from "react";
import ArrowLeftRight from "lucide-react/dist/esm/icons/arrow-left-right.js";
import Check from "lucide-react/dist/esm/icons/check.js";
import CircleCheck from "lucide-react/dist/esm/icons/circle-check.js";
import Pencil from "lucide-react/dist/esm/icons/pencil.js";
import Trash2 from "lucide-react/dist/esm/icons/trash-2.js";
import UserPlus from "lucide-react/dist/esm/icons/user-plus.js";
import Users from "lucide-react/dist/esm/icons/users.js";
import X from "lucide-react/dist/esm/icons/x.js";
import type { SavedAccount } from "@/types";
import { useNotify } from "@/components/Notifier";

interface AccountsPanelProps {
  accounts: SavedAccount[];
  busy: boolean;
  onSwitch: (accountId: string) => void;
  onRemove: (accountId: string) => void;
  onRename: (accountId: string, label: string) => void;
  onAdd: () => void;
}

export function accountName(account: Pick<SavedAccount, "label" | "email" | "accountId">): string {
  return account.label || account.email || account.accountId;
}

function relativeTime(value?: string | null): string {
  if (!value) return "从未使用";
  const ms = Date.now() - Date.parse(value);
  if (!Number.isFinite(ms)) return "—";
  const minutes = Math.floor(ms / 60_000);
  if (minutes < 1) return "刚刚使用";
  if (minutes < 60) return `${minutes} 分钟前使用`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前使用`;
  return `${Math.floor(hours / 24)} 天前使用`;
}

function shortId(accountId: string): string {
  return accountId.length > 14 ? `${accountId.slice(0, 8)}…${accountId.slice(-4)}` : accountId;
}

export function AccountsPanel({ accounts, busy, onSwitch, onRemove, onRename, onAdd }: AccountsPanelProps) {
  const [editing, setEditing] = useState<string | null>(null);
  const [draft, setDraft] = useState("");
  const { confirm } = useNotify();

  const startEdit = (account: SavedAccount) => {
    setEditing(account.accountId);
    setDraft(account.label ?? "");
  };
  const commitEdit = (accountId: string) => {
    onRename(accountId, draft);
    setEditing(null);
  };

  return (
    <section className="panel accounts-panel">
      <header>
        <div className="section-heading">
          <span className="section-icon"><Users size={19} /></span>
          <div>
            <h2>账号</h2>
            <p>{accounts.length ? `${accounts.length} 个账号 · 切换后立即生效，无需重启 Codex` : "登录后账号会自动保存在这里"}</p>
          </div>
        </div>
        <button className="billing-panel__refresh" type="button" disabled={busy} onClick={onAdd}>
          <UserPlus size={13} />
          添加账号
        </button>
      </header>
      {accounts.length ? (
        <ul className="accounts-list">
          {accounts.map((account) => {
            const name = accountName(account);
            return (
              <li key={account.accountId} className={account.active ? "accounts-item accounts-item--active" : "accounts-item"}>
                <span className="accounts-item__avatar" aria-hidden="true">{name.slice(0, 1).toUpperCase()}</span>
                <div className="accounts-item__main">
                  {editing === account.accountId ? (
                    <form
                      className="accounts-item__edit"
                      onSubmit={(event) => {
                        event.preventDefault();
                        commitEdit(account.accountId);
                      }}
                    >
                      <input
                        autoFocus
                        maxLength={40}
                        value={draft}
                        placeholder={account.email ?? "备注名"}
                        aria-label="账号备注名"
                        onChange={(event) => setDraft(event.target.value)}
                        onKeyDown={(event) => {
                          if (event.key === "Escape") setEditing(null);
                        }}
                      />
                      <button type="submit" aria-label="保存备注名"><Check size={14} /></button>
                      <button type="button" aria-label="取消" onClick={() => setEditing(null)}><X size={14} /></button>
                    </form>
                  ) : (
                    <strong>
                      {name}
                      {account.label && account.email ? <small>{account.email}</small> : null}
                    </strong>
                  )}
                  <span className="accounts-item__meta">
                    <code title={account.accountId}>{shortId(account.accountId)}</code>
                    {!account.refreshable ? <span className="accounts-tag accounts-tag--warm">Access Token · 不可自动刷新</span> : null}
                    {!account.usable ? <span className="accounts-tag accounts-tag--bad">凭据失效，请重新登录</span> : null}
                    <span>{account.active ? "当前使用中" : relativeTime(account.lastUsedAt)}</span>
                  </span>
                  <span className="accounts-item__env">
                    {account.deviceId ? <span title={account.deviceId}>设备 {account.deviceId.slice(0, 8)}</span> : <span>切换后分配独立设备</span>}
                    {account.network ? <span>{account.network}</span> : null}
                  </span>
                </div>
                <div className="accounts-item__actions">
                  {account.active ? (
                    <span className="accounts-item__current"><CircleCheck size={13} />使用中</span>
                  ) : (
                    <button
                      className="billing-panel__refresh"
                      type="button"
                      disabled={busy || !account.usable}
                      onClick={() => onSwitch(account.accountId)}
                    >
                      <ArrowLeftRight size={13} />
                      切换
                    </button>
                  )}
                  <button className="accounts-icon-button" type="button" aria-label={`重命名 ${name}`} title="备注名" onClick={() => startEdit(account)}>
                    <Pencil size={13} />
                  </button>
                  <button
                    className="accounts-icon-button accounts-icon-button--danger"
                    type="button"
                    aria-label={`删除 ${name}`}
                    title={account.active ? "正在使用的账号不能删除" : "删除账号"}
                    disabled={account.active || busy}
                    onClick={async () => {
                      const ok = await confirm({
                        title: `删除账号 ${name}？`,
                        message: "账号的凭据、虚拟设备和出站线路绑定都会删除，之后需要重新登录才能再次使用。",
                        confirmText: "删除",
                        danger: true,
                      });
                      if (ok) onRemove(account.accountId);
                    }}
                  >
                    <Trash2 size={13} />
                  </button>
                </div>
              </li>
            );
          })}
        </ul>
      ) : (
        <p className="accounts-empty">还没有保存的账号。点击「添加账号」登录 ChatGPT，账号会自动加入列表。</p>
      )}
    </section>
  );
}
