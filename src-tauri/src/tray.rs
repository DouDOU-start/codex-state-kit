//! Tray icon with a quick account switcher, modelled on cc-switch's tray.
//!
//! The menu lists saved accounts as check items (the active one checked),
//! followed by "show window" and "quit". Left-clicking the icon shows the
//! window; the menu opens on right-click (any click on Linux).

use std::path::PathBuf;

use codex_state_kit::accounts::{self, AccountView};
use serde::Serialize;
use tauri::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::state::AppState;
use crate::ui_language::UiLanguage;

pub const TRAY_ID: &str = "main";
/// Emitted after the tray switched accounts so the window can reload.
pub const ACCOUNTS_CHANGED: &str = "accounts-changed";
const ACCOUNT_PREFIX: &str = "account:";
const SHOW_ID: &str = "show";
const QUIT_ID: &str = "quit";
const APP_NAME: &str = "Codex State Kit";

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountsChanged {
    ok: bool,
    message: String,
}

fn display_name(account: &AccountView) -> String {
    account
        .email
        .clone()
        .unwrap_or_else(|| account.account_id.clone())
}

/// `&` marks a mnemonic in menu text; show it literally.
fn menu_text(text: &str) -> String {
    text.replace('&', "&&")
}

async fn codex_home<R: Runtime>(app: &AppHandle<R>) -> PathBuf {
    let core = app.state::<AppState>().core();
    let home = core.settings.lock().await.codex_home.clone();
    PathBuf::from(home)
}

fn build_menu<R: Runtime>(app: &AppHandle<R>, accounts: &[AccountView]) -> tauri::Result<Menu<R>> {
    let language = app.state::<UiLanguage>();
    let menu = Menu::new(app)?;
    menu.append(&MenuItem::with_id(
        app,
        "title",
        language.text("切换账号", "Сменить аккаунт"),
        false,
        None::<&str>,
    )?)?;
    if accounts.is_empty() {
        menu.append(&MenuItem::with_id(
            app,
            "empty",
            language.text("还没有保存的账号", "Нет сохранённых аккаунтов"),
            false,
            None::<&str>,
        )?)?;
    }
    for account in accounts {
        let name = display_name(account);
        let text = if account.usable {
            name
        } else {
            format!(
                "{name}{}",
                language.text("（凭据失效）", " (нужна авторизация)")
            )
        };
        menu.append(&CheckMenuItem::with_id(
            app,
            format!("{ACCOUNT_PREFIX}{}", account.account_id),
            menu_text(&text),
            account.usable,
            account.active,
            None::<&str>,
        )?)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(
        app,
        SHOW_ID,
        language.text("显示主窗口", "Показать окно"),
        true,
        None::<&str>,
    )?)?;
    menu.append(&MenuItem::with_id(
        app,
        QUIT_ID,
        format!("{} {APP_NAME}", language.text("退出", "Выйти из")),
        true,
        None::<&str>,
    )?)?;
    Ok(menu)
}

/// Rebuilds the tray menu and tooltip from an account list.
pub fn update<R: Runtime>(app: &AppHandle<R>, accounts: &[AccountView]) {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return;
    };
    match build_menu(app, accounts) {
        Ok(menu) => {
            if let Err(error) = tray.set_menu(Some(menu)) {
                eprintln!("[tray] 更新菜单失败: {error}");
            }
        }
        Err(error) => eprintln!("[tray] 构建菜单失败: {error}"),
    }
    let tooltip = match accounts.iter().find(|account| account.active) {
        Some(account) => format!("{APP_NAME} · {}", display_name(account)),
        None => APP_NAME.to_string(),
    };
    let _ = tray.set_tooltip(Some(tooltip));
}

/// Reloads saved accounts and rebuilds the tray menu.
pub async fn refresh<R: Runtime>(app: &AppHandle<R>) {
    let home = codex_home(app).await;
    match accounts::list(&home) {
        Ok(list) => update(app, &list),
        Err(error) => eprintln!("[tray] 读取账号失败: {error:#}"),
    }
}

fn show_main<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn switch_from_tray<R: Runtime>(app: &AppHandle<R>, account_id: String) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let home = codex_home(&app).await;
        let proxy = app.state::<AppState>().proxy.clone();
        let payload = match proxy.switch_account(&home, &account_id).await {
            Ok(status) => {
                let name = status.email.unwrap_or(account_id);
                let chinese = format!("已切换到 {name}，后续请求立即使用该账号");
                let russian = format!("Выбран {name}. Новые запросы сразу используют этот аккаунт");
                AccountsChanged {
                    ok: true,
                    message: app
                        .state::<UiLanguage>()
                        .text(&chinese, &russian)
                        .to_string(),
                }
            }
            Err(error) => AccountsChanged {
                ok: false,
                message: format!("{error:#}"),
            },
        };
        // Always rebuild: clicking a check item toggles its mark natively.
        refresh(&app).await;
        let _ = app.emit(ACCOUNTS_CHANGED, payload);
    });
}

fn on_menu_event<R: Runtime>(app: &AppHandle<R>, event: MenuEvent) {
    let id: &str = event.id().as_ref();
    match id {
        SHOW_ID => show_main(app),
        QUIT_ID => app.exit(0),
        _ => {
            if let Some(account_id) = id.strip_prefix(ACCOUNT_PREFIX) {
                switch_from_tray(app, account_id.to_string());
            }
        }
    }
}

pub fn create<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let menu = build_menu(app, &[])?;
    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(APP_NAME)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main(tray.app_handle());
            }
        });
    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    let handle = app.clone();
    tauri::async_runtime::spawn(async move { refresh(&handle).await });
    Ok(())
}
