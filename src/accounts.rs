//! Saved ChatGPT accounts for quick switching, modelled on cc-switch.
//!
//! The live Kit login stays in `auth.codex-state-kit.json`, which every
//! forwarded request reads.  This vault keeps a copy of each account's auth
//! next to it, so switching only replaces that file (and the overlaid Codex
//! `auth.json`).  Like cc-switch's backfill, the live login is written back
//! into the vault before every switch or new login, so tokens refreshed in
//! the meantime are never lost.  The vault lives in the Codex home and is not
//! touched when Kit restores the official Codex files on exit.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::identity::VmIdentity;
use crate::settings::OutboundMode;

use crate::login::{
    atomic_write, auth_sync_lock, kit_auth_path, overlay_kit_onto_official, read_auth_file,
    status_from_auth, LoginStatus,
};

pub const ACCOUNTS_FILE: &str = "accounts.codex-state-kit.json";
const VAULT_VERSION: u32 = 1;
const MAX_LABEL_CHARS: usize = 40;

static VAULT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn vault_lock() -> std::sync::MutexGuard<'static, ()> {
    VAULT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vault {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: Vec<StoredAccount>,
    /// The account whose environment is currently live in Kit's settings
    /// and virtual device. Survives restarts.
    #[serde(default)]
    environment_owner: Option<String>,
}

/// The outbound line an account uses.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkProfile {
    pub outbound_mode: OutboundMode,
    #[serde(default)]
    pub outbound_proxy: String,
    #[serde(default)]
    pub mihomo_subscription: String,
    #[serde(default)]
    pub mihomo_node: String,
    /// Selected node per subscription group (select groups only).
    #[serde(default)]
    pub mihomo_selections: BTreeMap<String, String>,
}

/// Everything upstream sees besides the credentials: the virtual device and
/// the outbound line. Each account keeps its own so switching accounts never
/// mixes one account's traffic with another's fingerprint or exit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountEnvironment {
    pub vm: VmIdentity,
    pub network: NetworkProfile,
}

impl AccountEnvironment {
    /// Compares the persisted parts (runtime session ids are ignored).
    pub fn same_as(&self, other: &Self) -> bool {
        self.network == other.network
            && serde_json::to_value(&self.vm).ok() == serde_json::to_value(&other.vm).ok()
    }
}

/// Stores `current` for an account. Group selections are only known while
/// the subscription core runs; keep the saved ones when none are reported.
fn store_environment(slot: &mut Option<AccountEnvironment>, current: &AccountEnvironment) {
    let mut next = current.clone();
    if next.network.mihomo_selections.is_empty() {
        if let Some(previous) = slot.as_ref() {
            next.network.mihomo_selections = previous.network.mihomo_selections.clone();
        }
    }
    *slot = Some(next);
}

/// What [`plan_environment`] decided.
pub struct EnvironmentPlan {
    pub account_id: String,
    /// The environment to make live before [`commit_environment`];
    /// `None` when the live environment already belongs to this account.
    pub target: Option<AccountEnvironment>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAccount {
    account_id: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    label: Option<String>,
    /// The full Kit auth file for this account.
    auth: Value,
    added_at: String,
    #[serde(default)]
    last_used_at: Option<String>,
    #[serde(default)]
    environment: Option<AccountEnvironment>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub account_id: String,
    pub email: Option<String>,
    pub label: Option<String>,
    pub auth_mode: Option<String>,
    /// Access-token imports cannot refresh and need re-importing on expiry.
    pub refreshable: bool,
    /// The saved credentials still look usable.
    pub usable: bool,
    pub active: bool,
    pub added_at: String,
    pub last_used_at: Option<String>,
    /// Installation id of the account's virtual device.
    pub device_id: Option<String>,
    /// Short description of the account's outbound line.
    pub network: Option<String>,
}

fn network_label(network: &NetworkProfile) -> String {
    match network.outbound_mode {
        OutboundMode::Manual => {
            let proxy = network.outbound_proxy.trim();
            if proxy.is_empty() {
                return "手动代理 · 未配置".into();
            }
            // Never show credentials or session parameters.
            let shown = url::Url::parse(proxy)
                .ok()
                .and_then(|url| {
                    let host = url.host_str()?.to_string();
                    Some(match url.port() {
                        Some(port) => format!("{}://{host}:{port}", url.scheme()),
                        None => format!("{}://{host}", url.scheme()),
                    })
                })
                .unwrap_or_else(|| "已配置".into());
            format!("手动代理 · {shown}")
        }
        OutboundMode::Mihomo => {
            let node = network
                .mihomo_selections
                .iter()
                .next()
                .map(|(group, node)| format!("{group} → {node}"))
                .or_else(|| {
                    let node = network.mihomo_node.trim();
                    (!node.is_empty()).then(|| node.to_string())
                })
                .unwrap_or_else(|| "自动".into());
            format!("订阅节点 · {node}")
        }
    }
}

pub fn vault_path(home: &Path) -> PathBuf {
    home.join(ACCOUNTS_FILE)
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// A corrupt vault is an error rather than an empty list, so a later save can
/// never silently overwrite saved accounts.
fn load(home: &Path) -> Result<Vault> {
    let path = vault_path(home);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("账号库 {} 已损坏，请检查或备份后删除", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vault::default()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn save(home: &Path, vault: &mut Vault) -> Result<()> {
    vault.version = VAULT_VERSION;
    let path = vault_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    atomic_write(&path, &serde_json::to_vec_pretty(vault)?)?;
    restrict_permissions(&path);
    Ok(())
}

/// The vault holds refresh tokens: keep it readable by the owner only.
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// The account currently used by Kit, read from the live Kit login file.
fn live_account(home: &Path) -> Option<(String, LoginStatus, Value)> {
    let auth = read_auth_file(&kit_auth_path(home)).ok()??;
    let status = status_from_auth(&auth);
    let account_id = status.account_id.clone().filter(|_| status.logged_in)?;
    Some((account_id, status, auth))
}

/// Backfills the live Kit login into the vault. Caller holds the vault lock.
fn capture_locked(home: &Path, vault: &mut Vault) -> bool {
    let Some((account_id, status, auth)) = live_account(home) else {
        return false;
    };
    match vault
        .accounts
        .iter_mut()
        .find(|account| account.account_id == account_id)
    {
        Some(account) => {
            if account.auth == auth && account.email == status.email {
                return false;
            }
            account.auth = auth;
            account.email = status.email.or(account.email.take());
        }
        None => {
            let at = now();
            vault.accounts.push(StoredAccount {
                account_id,
                email: status.email,
                label: None,
                auth,
                added_at: at.clone(),
                last_used_at: Some(at),
                environment: None,
            });
        }
    }
    true
}

/// Saves the live Kit login into the vault.
pub fn capture(home: &Path) -> Result<()> {
    let _guard = vault_lock();
    let mut vault = load(home)?;
    if capture_locked(home, &mut vault) {
        save(home, &mut vault)?;
    }
    Ok(())
}

/// [`capture`] for login paths, where a vault problem must not fail the login.
pub fn capture_quietly(home: &Path) {
    if let Err(error) = capture(home) {
        eprintln!("[accounts] 保存账号失败: {error:#}");
    }
}

/// Lists saved accounts, first importing the live Kit login if it is new.
pub fn list(home: &Path) -> Result<Vec<AccountView>> {
    let _guard = vault_lock();
    let mut vault = load(home)?;
    if capture_locked(home, &mut vault) {
        save(home, &mut vault)?;
    }
    let active = live_account(home).map(|(id, _, _)| id);
    Ok(vault
        .accounts
        .iter()
        .map(|account| {
            let status = status_from_auth(&account.auth);
            AccountView {
                account_id: account.account_id.clone(),
                email: account.email.clone().or(status.email),
                label: account.label.clone(),
                auth_mode: status.auth_mode,
                refreshable: status.refreshable,
                usable: status.logged_in,
                active: active.as_deref() == Some(account.account_id.as_str()),
                added_at: account.added_at.clone(),
                last_used_at: account.last_used_at.clone(),
                device_id: account
                    .environment
                    .as_ref()
                    .map(|env| env.vm.installation_id.clone()),
                network: account
                    .environment
                    .as_ref()
                    .map(|env| network_label(&env.network)),
            }
        })
        .collect())
}

/// Makes `account_id` the live Kit login.  Requests pick it up immediately:
/// every forwarded request reads the Kit login file.
pub fn switch(home: &Path, account_id: &str) -> Result<LoginStatus> {
    let _guard = vault_lock();
    let mut vault = load(home)?;
    capture_locked(home, &mut vault);
    let Some(target) = vault
        .accounts
        .iter_mut()
        .find(|account| account.account_id == account_id)
    else {
        bail!("账号不存在");
    };
    let status = status_from_auth(&target.auth);
    if !status.logged_in {
        bail!("该账号保存的凭据已失效，请重新登录");
    }
    target.last_used_at = Some(now());
    let auth = serde_json::to_vec_pretty(&target.auth)?;
    // Persist the backfilled outgoing account before the live file changes,
    // so a failed save can never lose its latest tokens.
    save(home, &mut vault)?;
    {
        let _auth = auth_sync_lock();
        atomic_write(&kit_auth_path(home), &auth)?;
    }
    overlay_kit_onto_official(home)?;
    Ok(status)
}

/// Deletes a saved account.  The account in use cannot be deleted.
pub fn remove(home: &Path, account_id: &str) -> Result<()> {
    let _guard = vault_lock();
    let mut vault = load(home)?;
    capture_locked(home, &mut vault);
    if live_account(home).is_some_and(|(id, _, _)| id == account_id) {
        bail!("不能删除正在使用的账号，请先切换到其他账号");
    }
    let before = vault.accounts.len();
    vault
        .accounts
        .retain(|account| account.account_id != account_id);
    if vault.accounts.len() == before {
        bail!("账号不存在");
    }
    save(home, &mut vault)
}

/// Sets or clears (empty label) an account's display name.
pub fn rename(home: &Path, account_id: &str, label: &str) -> Result<()> {
    let label: String = label.trim().chars().take(MAX_LABEL_CHARS).collect();
    let _guard = vault_lock();
    let mut vault = load(home)?;
    capture_locked(home, &mut vault);
    let Some(account) = vault
        .accounts
        .iter_mut()
        .find(|account| account.account_id == account_id)
    else {
        bail!("账号不存在");
    };
    account.label = (!label.is_empty()).then_some(label);
    save(home, &mut vault)
}

/// First step of keeping each account's environment separate.
///
/// Saves the live environment (`current`) into the account that owns it and
/// decides what the live account should use: its saved environment, the
/// current one on first use (nothing was bound yet), or `fresh(current)`
/// (a new virtual device) when another account owned the live environment.
/// Ownership only moves in [`commit_environment`], after the target is live,
/// so a failed switch never records the wrong environment for an account.
pub fn plan_environment(
    home: &Path,
    current: &AccountEnvironment,
    fresh: impl FnOnce(&AccountEnvironment) -> AccountEnvironment,
) -> Result<Option<EnvironmentPlan>> {
    let _guard = vault_lock();
    let mut vault = load(home)?;
    capture_locked(home, &mut vault);
    let Some((live, _, _)) = live_account(home) else {
        return Ok(None);
    };
    let owner = vault.environment_owner.clone();
    if let Some(owner) = owner.as_deref() {
        if let Some(account) = vault.accounts.iter_mut().find(|a| a.account_id == owner) {
            store_environment(&mut account.environment, current);
        }
    }
    let target = if owner.as_deref() == Some(live.as_str()) {
        None
    } else {
        let saved = vault
            .accounts
            .iter()
            .find(|account| account.account_id == live)
            .and_then(|account| account.environment.clone());
        Some(match saved {
            Some(saved) => saved,
            None if owner.is_none() => current.clone(),
            None => fresh(current),
        })
    };
    save(home, &mut vault)?;
    Ok(Some(EnvironmentPlan {
        account_id: live,
        target,
    }))
}

/// Second step: records `environment` as the account's own and marks it as
/// the owner of the live environment.
pub fn commit_environment(
    home: &Path,
    account_id: &str,
    environment: &AccountEnvironment,
) -> Result<()> {
    let _guard = vault_lock();
    let mut vault = load(home)?;
    let Some(account) = vault
        .accounts
        .iter_mut()
        .find(|account| account.account_id == account_id)
    else {
        bail!("账号不存在");
    };
    store_environment(&mut account.environment, environment);
    vault.environment_owner = Some(account_id.to_string());
    save(home, &mut vault)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    fn jwt(account: &str, email: &str) -> String {
        let encode = |value: Value| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value.to_string())
        };
        format!(
            "{}.{}.sig",
            encode(json!({"alg": "none"})),
            encode(json!({
                "email": email,
                "https://api.openai.com/auth": { "chatgpt_account_id": account }
            }))
        )
    }

    fn login(home: &Path, account: &str, refresh: &str) {
        let token = jwt(account, &format!("{account}@example.com"));
        let auth = json!({
            "auth_mode": "chatgpt",
            "tokens": {
                "id_token": token, "access_token": token,
                "refresh_token": refresh, "account_id": account
            }
        });
        atomic_write(&kit_auth_path(home), auth.to_string().as_bytes()).unwrap();
    }

    fn live_refresh(home: &Path) -> String {
        read_auth_file(&kit_auth_path(home)).unwrap().unwrap()["tokens"]["refresh_token"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn imports_the_live_login_and_switches_between_accounts() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert!(list(home).unwrap().is_empty());

        login(home, "acct-a", "rt-a");
        capture(home).unwrap();
        login(home, "acct-b", "rt-b");
        let accounts = list(home).unwrap();
        assert_eq!(accounts.len(), 2);
        assert!(accounts
            .iter()
            .any(|a| a.account_id == "acct-b" && a.active));
        assert_eq!(
            accounts
                .iter()
                .find(|a| a.account_id == "acct-a")
                .unwrap()
                .email
                .as_deref(),
            Some("acct-a@example.com")
        );

        let status = switch(home, "acct-a").unwrap();
        assert_eq!(status.account_id.as_deref(), Some("acct-a"));
        assert_eq!(live_refresh(home), "rt-a");
        // The overlaid Codex auth.json follows the switch.
        let official = read_auth_file(&home.join("auth.json")).unwrap().unwrap();
        assert_eq!(official["tokens"]["refresh_token"], "rt-a");
        assert!(list(home)
            .unwrap()
            .iter()
            .any(|a| a.account_id == "acct-a" && a.active));
    }

    #[test]
    fn switching_backfills_tokens_refreshed_in_the_meantime() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        login(home, "acct-a", "rt-a");
        capture(home).unwrap();
        login(home, "acct-b", "rt-b");
        capture(home).unwrap();
        // Codex rotated acct-b's refresh token after it was saved.
        login(home, "acct-b", "rt-b-rotated");
        switch(home, "acct-a").unwrap();
        switch(home, "acct-b").unwrap();
        assert_eq!(live_refresh(home), "rt-b-rotated");
    }

    #[test]
    fn active_account_cannot_be_removed_and_labels_are_trimmed() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        login(home, "acct-a", "rt-a");
        capture(home).unwrap();
        login(home, "acct-b", "rt-b");
        assert!(remove(home, "acct-b").is_err());
        rename(home, "acct-a", "  工作号  ").unwrap();
        let label = list(home)
            .unwrap()
            .into_iter()
            .find(|a| a.account_id == "acct-a")
            .unwrap()
            .label;
        assert_eq!(label.as_deref(), Some("工作号"));
        remove(home, "acct-a").unwrap();
        assert_eq!(list(home).unwrap().len(), 1);
        assert!(switch(home, "acct-a").is_err());
    }

    fn env(installation: &str, proxy: &str) -> AccountEnvironment {
        let mut vm = VmIdentity::ephemeral();
        vm.installation_id = installation.into();
        AccountEnvironment {
            vm,
            network: NetworkProfile {
                outbound_proxy: proxy.into(),
                ..NetworkProfile::default()
            },
        }
    }

    fn fresh(current: &AccountEnvironment) -> AccountEnvironment {
        let mut next = current.clone();
        next.vm.installation_id = "fresh".into();
        next
    }

    /// Runs plan + commit the way ProxyHandle does, returning the new live env.
    fn sync(home: &Path, live: &AccountEnvironment) -> AccountEnvironment {
        let plan = plan_environment(home, live, fresh).unwrap().unwrap();
        let next = plan.target.unwrap_or_else(|| live.clone());
        commit_environment(home, &plan.account_id, &next).unwrap();
        next
    }

    #[test]
    fn each_account_keeps_its_own_device_and_network() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        login(home, "acct-a", "rt-a");
        // First binding adopts the environment already in use.
        let mut live = sync(home, &env("dev-a", "socks5://a:1"));
        assert_eq!(live.vm.installation_id, "dev-a");

        // A new login gets a new virtual device, not acct-a's.
        login(home, "acct-b", "rt-b");
        live = sync(home, &live);
        assert_eq!(live.vm.installation_id, "fresh");
        // acct-b changes its own line while active.
        live.network.outbound_proxy = "socks5://b:2".into();
        live = sync(home, &live);

        switch(home, "acct-a").unwrap();
        live = sync(home, &live);
        assert_eq!(live.vm.installation_id, "dev-a");
        assert_eq!(live.network.outbound_proxy, "socks5://a:1");

        switch(home, "acct-b").unwrap();
        live = sync(home, &live);
        assert_eq!(live.vm.installation_id, "fresh");
        assert_eq!(live.network.outbound_proxy, "socks5://b:2");

        let accounts = list(home).unwrap();
        let b = accounts.iter().find(|a| a.account_id == "acct-b").unwrap();
        assert_eq!(b.device_id.as_deref(), Some("fresh"));
        assert_eq!(b.network.as_deref(), Some("手动代理 · socks5://b:2"));
    }

    #[test]
    fn an_uncommitted_switch_does_not_move_ownership() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        login(home, "acct-a", "rt-a");
        let live = sync(home, &env("dev-a", "socks5://a:1"));
        login(home, "acct-b", "rt-b");
        // Planning without committing (the apply failed) keeps acct-a as owner,
        // so the unchanged live environment is saved back to acct-a again.
        plan_environment(home, &live, fresh).unwrap();
        let plan = plan_environment(home, &live, fresh).unwrap().unwrap();
        assert_eq!(plan.target.unwrap().vm.installation_id, "fresh");
        switch(home, "acct-a").unwrap();
        let back = sync(home, &live);
        assert_eq!(back.vm.installation_id, "dev-a");
    }

    #[test]
    fn subscription_selections_survive_when_the_core_is_stopped() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        login(home, "acct-a", "rt-a");
        let mut live = env("dev-a", "");
        live.network.outbound_mode = OutboundMode::Mihomo;
        live.network
            .mihomo_selections
            .insert("Kit".into(), "HK-01".into());
        live = sync(home, &live);
        let mut stopped = live.clone();
        stopped.network.mihomo_selections.clear();
        sync(home, &stopped);
        let a = list(home).unwrap().into_iter().next().unwrap();
        assert_eq!(a.network.as_deref(), Some("订阅节点 · Kit → HK-01"));
    }

    #[test]
    fn a_corrupt_vault_is_reported_instead_of_overwritten() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        std::fs::write(vault_path(home), b"{broken").unwrap();
        login(home, "acct-a", "rt-a");
        assert!(list(home).is_err());
        assert_eq!(std::fs::read(vault_path(home)).unwrap(), b"{broken");
    }

    #[test]
    fn vault_survives_restarts() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        login(home, "acct-a", "rt-a");
        capture(home).unwrap();
        login(home, "acct-b", "rt-b");
        capture(home).unwrap();
        // A restart only re-reads files: nothing is kept in memory.
        let raw: Value = serde_json::from_slice(&std::fs::read(vault_path(home)).unwrap()).unwrap();
        assert_eq!(raw["version"], 1);
        assert_eq!(raw["accounts"].as_array().unwrap().len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(vault_path(home))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
