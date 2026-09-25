use codex_state_kit::accounts::{self, AccountView};
use codex_state_kit::billing::{BillingSummary, PricingRuleSpec, UsageFilter, UsageRecordsPage};
use codex_state_kit::pricing::{CatalogInfo, ModelPriceRow};
use codex_state_kit::{
    exchange_refresh_token, import_access_token as persist_access_token, inspect_codex_config,
    login_http_client_via, login_status, persist_refresh_token_import, poll_device_login,
    start_device_login, token_import_http_client_via, CodexConfigView, LoginEndpoints, LoginStart,
    LoginStatus, SettingsPatch, Status,
};
use serde::Serialize;
use std::path::PathBuf;
use tauri::State;

use crate::error::{command, CommandResult};
use crate::state::{AppState, LoginSession};
use codex_state_kit::{browser_login::BrowserLogin, login::LoginMethod};
use std::sync::Arc;

#[tauri::command]
pub async fn set_ui_language(app: tauri::AppHandle, language: String) -> CommandResult<()> {
    use tauri::Manager;
    app.state::<crate::ui_language::UiLanguage>()
        .set(&language)?;
    crate::tray::refresh(&app).await;
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginPoll {
    pub status: String,
    pub message: Option<String>,
    pub login: Option<LoginStatus>,
}

#[tauri::command(async)]
pub async fn probe_outbound_latency(
    state: State<'_, AppState>,
    kind: String,
    proxy: Option<String>,
) -> CommandResult<codex_state_kit::latency::LatencyReport> {
    command(state.proxy.probe_latency(&kind, proxy).await)
}

#[tauri::command(async)]
pub async fn mihomo_groups(
    state: State<'_, AppState>,
) -> CommandResult<Vec<codex_state_kit::mihomo::ProxyGroup>> {
    command(state.proxy.app().mihomo.list_groups().await)
}

#[tauri::command(async)]
pub async fn mihomo_select(
    state: State<'_, AppState>,
    group: String,
    node: String,
) -> CommandResult<()> {
    command(state.proxy.select_mihomo_node(&group, &node).await)
}

#[tauri::command(async)]
pub async fn update_vm_identity(
    state: State<'_, AppState>,
    profile: codex_state_kit::identity::VmProfile,
) -> CommandResult<Status> {
    command(state.proxy.update_vm_identity(profile).await)
}

#[tauri::command(async)]
pub async fn regenerate_vm_installation_id(state: State<'_, AppState>) -> CommandResult<Status> {
    command(state.proxy.regenerate_vm_installation_id().await)
}

#[tauri::command(async)]
pub async fn detect_vm_cli_version(state: State<'_, AppState>) -> CommandResult<Status> {
    command(state.proxy.detect_vm_cli_version().await)
}

#[tauri::command(async)]
pub async fn mihomo_group_delay(
    state: State<'_, AppState>,
    group: String,
    node: Option<String>,
) -> CommandResult<Vec<codex_state_kit::latency::LatencySample>> {
    let target = codex_state_kit::latency::NODE_PROBE_TARGET;
    if let Some(node) = node {
        let group = group.clone();
        let node = node.clone();
        return command(
            state
                .proxy
                .app()
                .mihomo
                .probe_node_delay(&group, &node, target)
                .await,
        )
        .map(|sample| vec![sample]);
    }
    command(
        state
            .proxy
            .app()
            .mihomo
            .probe_group_delays(&group, &target)
            .await,
    )
}

#[tauri::command(async)]
pub async fn open_github_repo() -> CommandResult<()> {
    open::that("https://github.com/DouDOU-start/codex-state-kit").map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn check_update(
    app: tauri::AppHandle,
) -> CommandResult<codex_state_kit::update::UpdateInfo> {
    command(codex_state_kit::update::check_update(&app.package_info().version.to_string()).await)
}

#[tauri::command]
pub async fn open_release_page(tag: Option<String>) -> CommandResult<()> {
    let url = command(codex_state_kit::update::release_url(tag.as_deref()))?;
    open::that(url).map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn prepare_update(state: tauri::State<'_, AppState>) -> CommandResult<()> {
    command(state.prepare_update().await)
}

#[tauri::command]
pub async fn resume_after_update_failure(state: tauri::State<'_, AppState>) -> CommandResult<()> {
    state.resume_after_update_failure();
    Ok(())
}

#[tauri::command]
pub async fn restart_after_update(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> CommandResult<()> {
    if !state.update_is_prepared() {
        return Err("尚未准备更新安装".into());
    }
    app.restart();
}

#[tauri::command(async)]
pub async fn get_status(state: State<'_, AppState>) -> CommandResult<Status> {
    Ok(state.proxy.managed_status().await)
}

/// Read durable per-account usage totals.  The billing store is independent
/// from the bounded network log, so this remains available after a restart.
#[tauri::command(async)]
pub async fn get_billing_summary(
    state: State<'_, AppState>,
    from: Option<String>,
    to: Option<String>,
) -> CommandResult<BillingSummary> {
    command(state.core().billing.account_summaries(UsageFilter {
        from,
        to,
        ..UsageFilter::default()
    }))
}

/// Changes whenever usage records are written; the UI polls this cheaply
/// and reloads records and summaries only when it moves.
#[tauri::command(async)]
pub async fn get_billing_revision(state: State<'_, AppState>) -> CommandResult<u64> {
    Ok(state.core().billing.revision())
}

/// Query persisted request-level billing records with optional account/time,
/// source and model filters.
#[tauri::command(async)]
pub async fn get_billing_records(
    state: State<'_, AppState>,
    account_id: Option<String>,
    from: Option<String>,
    to: Option<String>,
    source: Option<String>,
    model: Option<String>,
    downgraded: Option<bool>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> CommandResult<UsageRecordsPage> {
    command(state.core().billing.list_usage(UsageFilter {
        account_id,
        from,
        to,
        source,
        model,
        downgraded,
        limit,
        offset,
    }))
}

/// Add or replace a model price snapshot. Existing usage rows keep the rule
/// selected when they were settled, so changing a price never rewrites history.
#[tauri::command(async)]
pub async fn set_billing_pricing(
    state: State<'_, AppState>,
    rule: PricingRuleSpec,
) -> CommandResult<i64> {
    command(state.core().billing.add_pricing_rule(rule))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingView {
    pub info: CatalogInfo,
    pub models: Vec<ModelPriceRow>,
}

fn pricing_view(state: &AppState) -> PricingView {
    let core = state.core();
    let pricing = core.billing.pricing();
    PricingView {
        info: pricing.info(),
        models: pricing.rows(),
    }
}

/// The live model price catalog (bundled, cached or synced from remote).
#[tauri::command(async)]
pub async fn get_pricing(state: State<'_, AppState>) -> CommandResult<PricingView> {
    Ok(pricing_view(&state))
}

/// Checks the remote price catalog now instead of waiting for the next tick.
#[tauri::command(async)]
pub async fn sync_pricing(state: State<'_, AppState>) -> CommandResult<PricingView> {
    command(state.proxy.sync_pricing().await)?;
    Ok(pricing_view(&state))
}

#[tauri::command(async)]
pub async fn set_config(
    state: State<'_, AppState>,
    settings: SettingsPatch,
) -> CommandResult<Status> {
    if settings.codex_home.trim() != state.core().settings.lock().await.codex_home {
        state.pending_login.lock().expect("pending login").cancel();
    }
    command(state.proxy.apply_settings(settings).await)
}

#[tauri::command(async)]
pub async fn get_codex_config(
    state: State<'_, AppState>,
    home: Option<String>,
) -> CommandResult<CodexConfigView> {
    let settings = state.core().settings.lock().await.clone();
    let home = home.unwrap_or(settings.codex_home);
    let suggested = format!("http://{}", settings.proxy_listen);
    Ok(inspect_codex_config(
        std::path::Path::new(&home),
        &suggested,
    ))
}

#[tauri::command(async)]
pub async fn get_login_status(
    state: State<'_, AppState>,
    home: Option<String>,
) -> CommandResult<LoginStatus> {
    let settings = state.core().settings.lock().await.clone();
    let home = home.unwrap_or(settings.codex_home);
    Ok(login_status(std::path::Path::new(&home)))
}

async fn codex_home(state: &AppState, home: Option<String>) -> PathBuf {
    PathBuf::from(home.unwrap_or(state.core().settings.lock().await.codex_home.clone()))
}

/// Saved ChatGPT accounts; the live Kit login is imported on first call.
#[tauri::command(async)]
pub async fn list_accounts(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    home: Option<String>,
) -> CommandResult<Vec<AccountView>> {
    let home = codex_home(&state, home).await;
    // Also catches logins that happened outside the login commands.
    if let Err(error) = state.proxy.sync_account_environment().await {
        eprintln!("[accounts] 绑定账号环境失败: {error:#}");
    }
    let list = command(accounts::list(&home))?;
    // A new login may have just been saved: keep the tray menu in sync.
    crate::tray::update(&app, &list);
    Ok(list)
}

/// Switches Kit to a saved account. Forwarded requests use it right away.
#[tauri::command(async)]
pub async fn switch_account(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    home: Option<String>,
    account_id: String,
) -> CommandResult<LoginStatus> {
    let home = codex_home(&state, home).await;
    let status = command(state.proxy.switch_account(&home, &account_id).await)?;
    crate::tray::refresh(&app).await;
    Ok(status)
}

#[tauri::command(async)]
pub async fn remove_account(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    home: Option<String>,
    account_id: String,
) -> CommandResult<()> {
    let home = codex_home(&state, home).await;
    command(accounts::remove(&home, &account_id))?;
    crate::tray::refresh(&app).await;
    Ok(())
}

#[tauri::command(async)]
pub async fn import_chatgpt_refresh_token(
    state: State<'_, AppState>,
    home: Option<String>,
    refresh_token: String,
    account_id: Option<String>,
) -> CommandResult<LoginStatus> {
    let settings = state.core().settings.lock().await.clone();
    let home = PathBuf::from(home.unwrap_or(settings.codex_home));
    let generation = {
        let mut slot = state.pending_login.lock().expect("pending login");
        if slot.closed {
            return Err("应用正在退出".into());
        }
        slot.cancel();
        slot.generation
    };
    let client = command(token_import_http_client_via(
        &state.core().login_proxy(account_id.as_deref()).await,
    ))?;
    let tokens = command(exchange_refresh_token(&client, &refresh_token).await)?;
    let status = {
        let slot = state.pending_login.lock().expect("pending login");
        if slot.generation != generation || slot.closed {
            return Err("登录已取消".into());
        }
        command(persist_refresh_token_import(&home, &refresh_token, &tokens))?
    };
    bind_new_login(&state).await;
    Ok(status)
}

/// A login becomes the live account: a new one gets its own environment, a
/// re-authorized one gets its saved environment back.
async fn bind_new_login(state: &AppState) {
    if let Err(error) = state.proxy.sync_account_environment().await {
        eprintln!("[accounts] 绑定账号环境失败: {error:#}");
    }
}

#[tauri::command(async)]
pub async fn import_chatgpt_access_token(
    state: State<'_, AppState>,
    home: Option<String>,
    access_token: String,
) -> CommandResult<LoginStatus> {
    let settings = state.core().settings.lock().await.clone();
    let home = PathBuf::from(home.unwrap_or(settings.codex_home));
    let status = {
        let mut slot = state.pending_login.lock().expect("pending login");
        if slot.closed {
            return Err("应用正在退出".into());
        }
        slot.cancel();
        command(persist_access_token(&home, &access_token))?
    };
    bind_new_login(&state).await;
    Ok(status)
}

#[tauri::command(async)]
pub async fn start_chatgpt_login(
    state: State<'_, AppState>,
    home: Option<String>,
    method: Option<LoginMethod>,
    account_id: Option<String>,
) -> CommandResult<LoginStart> {
    let settings = state.core().settings.lock().await.clone();
    let home = PathBuf::from(home.unwrap_or(settings.codex_home));
    // Sign in over the line this account is (or will be) bound to.
    let proxy = state.core().login_proxy(account_id.as_deref()).await;
    let generation = {
        let mut slot = state.pending_login.lock().expect("pending login");
        if slot.closed {
            return Err("应用正在退出".into());
        }
        slot.cancel();
        slot.proxy = proxy.clone();
        if matches!(method.unwrap_or_default(), LoginMethod::Browser) {
            let (start, pending) =
                BrowserLogin::start(home, &proxy).map_err(|err| err.to_string())?;
            slot.pending = Some(LoginSession::Browser(Arc::new(pending)));
            return Ok(start);
        }
        slot.generation
    };
    let client = command(login_http_client_via(&proxy))?;
    match start_device_login(&client, &LoginEndpoints::default(), home).await {
        Ok((start, pending)) => {
            let mut slot = state.pending_login.lock().expect("pending login");
            if slot.generation != generation || slot.closed {
                pending.cancel();
                return Err("登录已取消".into());
            }
            slot.pending = Some(LoginSession::Device(pending));
            Ok(start)
        }
        Err(err) => Err(err.to_string()),
    }
}

#[tauri::command(async)]
pub async fn poll_chatgpt_login(state: State<'_, AppState>) -> CommandResult<LoginPoll> {
    let (generation, pending, proxy) = {
        let guard = state.pending_login.lock().expect("pending login lock");
        (guard.generation, guard.pending.clone(), guard.proxy.clone())
    };
    let Some(pending) = pending else {
        return Ok(LoginPoll {
            status: "error".into(),
            message: Some("没有进行中的登录".into()),
            login: None,
        });
    };
    let result = match pending {
        LoginSession::Browser(pending) => Ok(pending.poll()),
        LoginSession::Device(pending) => {
            let client = command(login_http_client_via(&proxy))?;
            poll_device_login(&client, &LoginEndpoints::default(), &pending).await
        }
    };
    let poll = {
        let mut slot = state.pending_login.lock().expect("pending login");
        if slot.generation != generation {
            return Err("登录已取消".into());
        }
        match result {
            Ok(result) => {
                if result.status != codex_state_kit::PollStatus::Pending {
                    slot.cancel();
                }
                LoginPoll {
                    status: result.status.as_str().to_string(),
                    message: result.message,
                    login: result.login,
                }
            }
            Err(err) => {
                slot.cancel();
                return Err(err.to_string());
            }
        }
    };
    if poll.login.as_ref().is_some_and(|login| login.logged_in) {
        bind_new_login(&state).await;
    }
    Ok(poll)
}

#[tauri::command(async)]
pub async fn cancel_chatgpt_login(state: State<'_, AppState>) -> CommandResult<ActionResult> {
    state
        .pending_login
        .lock()
        .expect("pending login lock")
        .cancel();
    Ok(ActionResult {
        ok: true,
        message: "已取消登录".into(),
    })
}

#[tauri::command(async)]
pub async fn open_url(url: String) -> CommandResult<ActionResult> {
    let url = url.trim();
    if !url.starts_with("https://auth.openai.com/") {
        return Err("只能打开 ChatGPT 登录页".into());
    }
    open::that(url).map_err(|err| err.to_string())?;
    Ok(ActionResult {
        ok: true,
        message: "已打开登录页".into(),
    })
}
