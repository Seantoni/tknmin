//! Tokens — local AI token-usage tracking for macOS.
//!
//! Layering, outermost first. Each layer may depend only on the ones below it:
//!
//! - [`commands`] — the Tauri surface React calls
//! - [`state`] — what the running application holds
//! - [`pipeline`] — drafts in, stored records out
//! - [`repository`] — storage and aggregation behind one trait
//! - [`normalize`] — the only producer of validated [`domain::UsageRecord`]s
//! - [`adapters`] — one per source application, producing drafts
//! - [`domain`] — the shared vocabulary, depending on nothing
//!
//! [`fixtures`] sits beside the adapters: it produces the same drafts they
//! do, from a fixed table rather than from disk. Tests use it; the running
//! application imports real logs instead.

pub mod adapters;
pub mod alerts;
pub mod commands;
pub mod domain;
pub mod error;
pub mod fixtures;
pub mod menubar;
pub mod normalize;
pub mod pipeline;
pub mod prefs;
pub mod refresh;
pub mod repository;
pub mod state;

use std::time::Duration;

use tauri::{Emitter, Manager};
use tauri_plugin_notification::NotificationExt;

use state::AppState;

/// How often to refresh allowance windows. This is intentionally much more
/// frequent than a full log import: quota checks only inspect the freshest
/// local snapshots (plus Cursor's usage endpoint), so warnings stay timely
/// without repeatedly walking the full log history.
const QUOTA_POLL_INTERVAL: Duration = Duration::from_secs(60);

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let state = AppState::in_memory();
    let import_repository = state.repository_handle();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .manage(state)
        .setup(|app| {
            menubar::install(app.handle())?;

            let handle = app.handle().clone();
            if let Ok(config_dir) = handle.path().app_config_dir() {
                let options_path = prefs::options_path(config_dir);
                if let Some(state) = handle.try_state::<AppState>() {
                    state.set_options_path(options_path.clone());
                    if let Ok(options) = prefs::load_options(&options_path) {
                        state.replace_options(options);
                    }
                }
            }

            // Ask once up front so the first threshold crossing can notify.
            let _ = handle.notification().request_permission();

            // The first import walks hundreds of megabytes of logs, which is
            // far too slow to block the window on. It runs in the background;
            // the interface starts empty and fills in when the event lands.
            let import_handle = handle.clone();
            std::thread::spawn(move || {
                let report = refresh::refresh_all(import_repository.as_ref());
                if let Some(state) = import_handle.try_state::<AppState>() {
                    state.set_quotas(report.quotas.clone());
                }
                menubar::refresh(&import_handle);
                alerts::evaluate_and_notify(&import_handle);
                let _ = import_handle.emit("usage-imported", &report);
            });

            // Light quota poll so thresholds fire without a manual refresh.
            let poll_handle = handle.clone();
            std::thread::spawn(move || loop {
                std::thread::sleep(QUOTA_POLL_INTERVAL);
                let quotas = refresh::refresh_quotas();
                if let Some(state) = poll_handle.try_state::<AppState>() {
                    state.set_quotas(quotas.clone());
                }
                let _ = poll_handle.emit("quotas-updated", &quotas);
                menubar::refresh(&poll_handle);
                alerts::evaluate_and_notify(&poll_handle);
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::usage_summary,
            commands::recent_usage,
            commands::usage_record_count,
            commands::usage_sources,
            commands::usage_quota,
            commands::refresh_logs,
            commands::get_options,
            commands::set_options,
            commands::cursor_connection_status,
            commands::connect_cursor_dashboard,
            commands::disconnect_cursor_dashboard,
            commands::active_alerts,
            commands::snooze_alert,
            commands::handoff_prompt,
            commands::test_notification,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
