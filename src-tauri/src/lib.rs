mod commands;
mod error;
mod state;
mod tray;
mod window_shape;

use tauri::{Manager, RunEvent};

use codex_state_kit::mihomo::MihomoPaths;
use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be registered first: a second launch exits here before it can
        // re-attach Codex, back up auth.json or bind the proxy port again,
        // and the running Kit's window is brought to the front instead.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let resource_dir = app.path().resource_dir()?;
            let mihomo_name = if cfg!(windows) {
                "mihomo.exe"
            } else {
                "mihomo"
            };
            let development_mihomo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("resources/mihomo")
                .join(mihomo_name);
            let packaged_mihomo = resource_dir.join("mihomo").join(mihomo_name);
            let mihomo_binary = if cfg!(debug_assertions) && development_mihomo.is_file() {
                development_mihomo
            } else {
                packaged_mihomo
            };
            let mut mihomo_data = app.path().app_local_data_dir()?;
            if cfg!(debug_assertions) {
                mihomo_data.push("dev");
            }
            // The built-in WARP line is gone; drop its device key and logs.
            let _ = std::fs::remove_dir_all(mihomo_data.join("warp"));
            mihomo_data.push("mihomo");
            let state = AppState::initialize(MihomoPaths {
                bundled_binary: mihomo_binary,
                data_dir: mihomo_data,
            })
            .map_err(|err| err.to_string())?;
            state.start_runtime();
            app.manage(state);
            if let Some(window) = app.get_webview_window("main") {
                window_shape::round_corners(&window);
            }
            if let Err(error) = tray::create(app.handle()) {
                // The tray is a convenience: never block startup on it.
                eprintln!("[tray] 创建托盘失败: {error}");
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_billing_summary,
            commands::get_billing_records,
            commands::get_billing_revision,
            commands::set_billing_pricing,
            commands::get_pricing,
            commands::sync_pricing,
            commands::set_config,
            commands::get_codex_config,
            commands::get_login_status,
            commands::list_accounts,
            commands::switch_account,
            commands::remove_account,
            commands::import_chatgpt_refresh_token,
            commands::import_chatgpt_access_token,
            commands::start_chatgpt_login,
            commands::poll_chatgpt_login,
            commands::cancel_chatgpt_login,
            commands::open_url,
            commands::probe_outbound_latency,
            commands::mihomo_groups,
            commands::mihomo_select,
            commands::mihomo_group_delay,
            commands::update_vm_identity,
            commands::regenerate_vm_installation_id,
            commands::detect_vm_cli_version,
            commands::open_github_repo,
            commands::check_update,
            commands::open_release_page,
            commands::prepare_update,
            commands::resume_after_update_failure,
            commands::restart_after_update,
        ])
        .build(tauri::generate_context!())
        .expect("failed to build Codex State Kit")
        .run(|app, event| {
            if matches!(event, RunEvent::Exit | RunEvent::ExitRequested { .. }) {
                let state = app.state::<AppState>();
                state.restore_once();
            }
        });
}
