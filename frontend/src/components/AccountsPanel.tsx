import ArrowLeftRight from "lucide-react/dist/esm/icons/arrow-left-right.js";
import CircleCheck from "lucide-react/dist/esm/icons/circle-check.js";
import KeyRound from "lucide-react/dist/esm/icons/key-round.js";
import Trash2 from "lucide-react/dist/esm/icons/trash-2.js";
import UserPlus from "lucide-react/dist/esm/icons/user-plus.js";
import Users from "lucide-react/dist/esm/icons/users.js";
import type { SavedAccount } from "@/types";
import { useNotify } from "@/components/Notifier";

interface AccountsPanelProps {
  accounts: SavedAccount[];
  busy: boolean;
  onSwitch: (accountId: string) => void;
  onRemove: (accountId: string) => void;
  /** Signs the account in again, e.g. after its authorization expired. */
  onReauthorize: (account: SavedAccount) => void;
  onAdd: () => void;
}

export function accountName(account: Pick<SavedAccount, "email" | "accountId">): string {
  return account.email || account.accountId;
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

export function AccountsPanel({ accounts, busy, onSwitch, onRemove, onReauthorize, onAdd }: AccountsPanelProps) {
  const { confirm } = useNotify();

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
                  <strong>{name}</strong>
                  <span className="accounts-item__meta">
                    <code title={account.accountId}>{shortId(account.accountId)}</code>
                    {!account.refreshable ? <span className="accounts-tag accounts-tag--warm">Access Token · 不可自动刷新</span> : null}
                    {!account.usable ? <span className="accounts-tag accounts-tag--bad">凭据失效，请重新授权</span> : null}
                    <span>{account.active ? "当前使用中" : relativeTime(account.lastUsedAt)}</span>
                  </span>
                  <span className="accounts-item__env">
                    {account.deviceId ? <span title={account.deviceId}>设备 {account.deviceId.slice(0, 8)}</span> : <span>切换后分配独立设备</span>}
                    {account.network ? <span>{account.network}</span> : null}
                  </span>
                </div>
                <div className="accounts-item__actions">
                  {!account.usable ? (
                    <button className="billing-panel__refresh" type="button" disabled={busy} onClick={() => onReauthorize(account)}>
                      <KeyRound size={13} />
                      重新授权
                    </button>
                  ) : account.active ? (
                    <span className="accounts-item__current"><CircleCheck size={13} />使用中</span>
                  ) : (
                    <button
                      className="billing-panel__refresh"
                      type="button"
                      disabled={busy}
                      onClick={() => onSwitch(account.accountId)}
                    >
                      <ArrowLeftRight size={13} />
                      切换
                    </button>
                  )}
                  {account.usable ? (
                    <button
                      className="accounts-icon-button"
                      type="button"
                      aria-label={`重新授权 ${name}`}
                      title="重新授权"
                      disabled={busy}
                      onClick={() => onReauthorize(account)}
                    >
                      <KeyRound size={13} />
                    </button>
                  ) : null}
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
