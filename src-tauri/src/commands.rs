use codex_state_kit::{
    inspect_codex_config, login_status, poll_device_login, start_device_login, CodexConfigView,
    LoginEndpoints, LoginStart, LoginStatus, SettingsPatch, Status,
};
use serde::Serialize;
use std::path::PathBuf;
use tauri::State;

use crate::error::{command, CommandResult};
use crate::state::{AppState, LoginSession};
use codex_state_kit::{browser_login::BrowserLogin, login::LoginMethod};
use std::sync::Arc;

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
pub async fn connect_warp(state: State<'_, AppState>, accept_terms: bool) -> CommandResult<Status> {
    command(state.proxy.connect_warp(accept_terms).await)
}

#[tauri::command(async)]
pub async fn stop_warp(state: State<'_, AppState>) -> CommandResult<Status> {
    Ok(state.proxy.stop_warp().await)
}

#[tauri::command(async)]
pub async fn open_warp_terms() -> CommandResult<()> {
    open::that("https://www.cloudflare.com/application/terms/").map_err(|err| err.to_string())
}

#[tauri::command(async)]
pub async fn open_github_repo() -> CommandResult<()> {
    open::that("https://github.com/DouDOU-start/codex-state-kit").map_err(|err| err.to_string())
}

#[tauri::command]
pub async fn check_update(app: tauri::AppHandle) -> CommandResult<codex_state_kit::update::UpdateInfo> {
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
pub async fn restart_after_update(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> CommandResult<()> {
    if !state.update_is_prepared() { return Err("尚未准备更新安装".into()); }
    app.restart();
}

#[tauri::command(async)]
pub async fn get_status(state: State<'_, AppState>) -> CommandResult<Status> {
    Ok(state.proxy.managed_status().await)
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
pub async fn refresh_turn_state(state: State<'_, AppState>) -> CommandResult<Status> {
    command(state.core().refresh_turn_state().await)
}

#[tauri::command(async)]
pub async fn start_manual_collection(
    state: State<'_, AppState>,
    model: String,
) -> CommandResult<Status> {
    command(state.core().start_manual_collection(&model).await)
}

#[tauri::command(async)]
pub async fn stop_manual_collection(
    state: State<'_, AppState>,
    model: String,
) -> CommandResult<Status> {
    command(state.core().stop_manual_collection(&model).await)
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

#[tauri::command(async)]
pub async fn start_chatgpt_login(
    state: State<'_, AppState>,
    home: Option<String>,
    method: Option<LoginMethod>,
) -> CommandResult<LoginStart> {
    let settings = state.core().settings.lock().await.clone();
    let home = PathBuf::from(home.unwrap_or(settings.codex_home));
    let generation = {
        let mut slot = state.pending_login.lock().expect("pending login");
        if slot.closed {
            return Err("应用正在退出".into());
        }
        slot.cancel();
        if matches!(method.unwrap_or_default(), LoginMethod::Browser) {
            let (start, pending) = BrowserLogin::start(home).map_err(|err| err.to_string())?;
            slot.pending = Some(LoginSession::Browser(Arc::new(pending)));
            return Ok(start);
        }
        slot.generation
    };
    match start_device_login(&state.core().login_http, &LoginEndpoints::default(), home).await {
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
    let (generation, pending) = {
        let guard = state.pending_login.lock().expect("pending login lock");
        (guard.generation, guard.pending.clone())
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
            poll_device_login(
                &state.core().login_http,
                &LoginEndpoints::default(),
                &pending,
            )
            .await
        }
    };
    let mut slot = state.pending_login.lock().expect("pending login");
    if slot.generation != generation {
        return Err("登录已取消".into());
    }
    match result {
        Ok(result) => {
            if result.status != codex_state_kit::PollStatus::Pending {
                slot.cancel();
            }
            Ok(LoginPoll {
                status: result.status.as_str().to_string(),
                message: result.message,
                login: result.login,
            })
        }
        Err(err) => {
            slot.cancel();
            Err(err.to_string())
        }
    }
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
pub async fn set_bound_token_len(
    state: State<'_, AppState>,
    len: Option<usize>,
) -> CommandResult<Status> {
    Ok(state.proxy.core().set_bound_token_len(len).await)
}

#[tauri::command(async)]
pub async fn set_model_bound_token_len(
    state: State<'_, AppState>,
    model: String,
    len: Option<usize>,
) -> CommandResult<Status> {
    Ok(state.proxy.core().set_model_bound_token_len(&model, len).await)
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
