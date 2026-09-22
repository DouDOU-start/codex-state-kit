mod commands;
mod error;
mod state;

use tauri::{Manager, RunEvent};

use codex_state_kit::mihomo::MihomoPaths;
use codex_state_kit::warp::WarpPaths;
use state::AppState;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let resource_dir = app.path().resource_dir()?;
            let binary_name = if cfg!(windows) { "usque.exe" } else { "usque" };
            let binary = if cfg!(debug_assertions) {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("resources/warp")
                    .join(binary_name)
            } else {
                resource_dir.join("warp").join(binary_name)
            };
            let mut data_dir = app.path().app_local_data_dir()?;
            if cfg!(debug_assertions) {
                data_dir.push("dev");
            }
            data_dir.push("warp");
            let mihomo_name = if cfg!(windows) { "mihomo.exe" } else { "mihomo" };
            let mihomo_binary = if cfg!(debug_assertions) {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("resources/mihomo")
                    .join(mihomo_name)
            } else {
                resource_dir.join("mihomo").join(mihomo_name)
            };
            let mut mihomo_data = app.path().app_local_data_dir()?;
            if cfg!(debug_assertions) {
                mihomo_data.push("dev");
            }
            mihomo_data.push("mihomo");
            let state = AppState::initialize(
                WarpPaths { binary, data_dir },
                MihomoPaths {
                    bundled_binary: mihomo_binary,
                    data_dir: mihomo_data,
                },
            )
            .map_err(|err| err.to_string())?;
            state.start_runtime();
            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::set_config,
            commands::refresh_turn_state,
            commands::get_codex_config,
            commands::get_login_status,
            commands::import_chatgpt_refresh_token,
            commands::import_chatgpt_access_token,
            commands::start_chatgpt_login,
            commands::poll_chatgpt_login,
            commands::cancel_chatgpt_login,
            commands::open_url,
            commands::set_bound_token_len,
            commands::set_model_bound_token_len,
            commands::probe_outbound_latency,
            commands::connect_warp,
            commands::stop_warp,
            commands::open_warp_terms,
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
