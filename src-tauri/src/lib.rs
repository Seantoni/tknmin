//! Tokens — local AI token-usage tracking for macOS.
//!
//! Layering, outermost first. Each layer may depend only on the ones below it:
//!
//! - [`commands`] — the Tauri surface React calls
//! - [`refresh`] — the single owner of when anything is read or written
//! - [`state`] — what the running application holds
//! - [`pipeline`] — drafts in, validated records out
//! - [`repository`] — durable storage, transactions, and revisions
//! - [`normalize`] — the only producer of validated [`domain::UsageRecord`]s
//! - [`adapters`] — one per source application, producing deltas on request
//! - [`domain`] — the shared vocabulary, depending on nothing
//!
//! [`fixtures`] sits beside the adapters: it produces the same drafts they
//! do, from a fixed table rather than from disk. Tests use it; the running
//! application imports real logs instead.
//!
//! This file starts exactly one coordinator and then stops talking about
//! refresh. It contains no refresh duration, no polling loop, no scan thread,
//! and no adapter call — each of those would be a second refresh owner.

pub mod adapters;
pub mod alerts;
pub mod commands;
pub mod domain;
pub mod error;
pub mod fixtures;
pub mod menubar;
pub mod mini;
pub mod normalize;
pub mod pipeline;
pub mod prefs;
pub mod refresh;
pub mod repository;
pub mod state;

use std::sync::Arc;

use tauri::{Emitter, Manager};

use refresh::{DataChanged, RefreshCoordinator, RefreshObserver, RefreshTrigger};
use repository::{SqliteUsageRepository, UsageWriter};
use state::AppState;

/// The one event the interface listens to for data changes.
pub const DATA_CHANGED_EVENT: &str = "data-changed";

/// Emitted after options are committed, so every window holding its own copy
/// can follow the save instead of writing a stale one back later.
pub const OPTIONS_CHANGED_EVENT: &str = "options-changed";

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // The persistent store, opened before anything else: the window
            // paints the last committed revision immediately, and catch-up
            // work happens behind it rather than in front of it.
            let repository = open_repository(&handle);
            let state = AppState::with_reader(repository.clone());

            if let Ok(config_dir) = handle.path().app_config_dir() {
                let options_path = prefs::options_path(config_dir);
                state.set_options_path(options_path.clone());
                if let Ok(options) = prefs::load_options(&options_path) {
                    state.replace_options(options);
                }
            }

            // The coordinator is handed the only production writer. Nothing
            // else is given one, which is what makes the single-owner rule a
            // property of the types rather than a convention.
            let refresh = RefreshCoordinator::new(
                repository,
                Arc::new(TauriObserver {
                    app: handle.clone(),
                }),
            )
            .start();
            state.attach_refresh(refresh.clone());
            app.manage(state);

            menubar::install(app.handle())?;

            // Built here, while the options that decide whether it floats are
            // already loaded, and hidden until the user minimizes into it.
            let always_on_top = handle
                .try_state::<AppState>()
                .map(|state| state.options().mini_always_on_top)
                .unwrap_or(true);
            mini::install(app.handle(), always_on_top)?;
            install_window_behavior(&handle);

            // Ask once up front via UNUserNotificationCenter so the first
            // threshold crossing can notify (and Settings → test works while
            // the window is focused).
            let _ = alerts::ensure_notification_auth();

            // The one and only push. Everything after this is a trigger.
            refresh.submit(RefreshTrigger::Startup);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::dashboard_snapshot,
            commands::usage_summary,
            commands::recent_usage,
            commands::usage_record_count,
            commands::source_capabilities,
            commands::usage_quota,
            commands::sync_now,
            commands::window_focused,
            commands::is_syncing,
            commands::get_options,
            commands::set_options,
            commands::cursor_connection_status,
            commands::connect_cursor_dashboard,
            commands::disconnect_cursor_dashboard,
            commands::active_alerts,
            commands::snooze_alert,
            commands::handoff_prompt,
            commands::test_notification,
            commands::show_mini_window,
            commands::show_dashboard_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Open the persistent store, falling back to memory if the disk refuses.
///
/// A store that cannot be opened is a bad day, not a dead application: the
/// logs on disk remain the authority, so an in-memory run still shows correct
/// numbers — it just has to re-read everything on the next launch.
fn open_repository(app: &tauri::AppHandle) -> Arc<dyn UsageWriter> {
    let opened = match app.path().app_data_dir() {
        Ok(directory) => SqliteUsageRepository::open(&directory.join("usage.sqlite3")),
        Err(error) => Err(repository::RepositoryError::Backend(error.to_string())),
    };

    match opened {
        Ok(repository) => Arc::new(repository),
        Err(error) => {
            eprintln!("tokens: falling back to an in-memory store: {error}");
            Arc::new(repository::InMemoryUsageRepository::new())
        }
    }
}

/// The single publication path.
///
/// Everything downstream of a commit happens here, in one place and in one
/// order: the interface is told, then the menu bar repaints, then the alerts
/// are re-evaluated. All three read the same committed revision, so they
/// cannot disagree — and none of them can trigger a read of its own.
struct TauriObserver {
    app: tauri::AppHandle,
}

impl RefreshObserver for TauriObserver {
    fn publish(&self, event: &DataChanged) {
        // The interface hears about every committed revision, because
        // freshness ages even when the numbers do not.
        let _ = self.app.emit(DATA_CHANGED_EVENT, event);

        // The menu bar and the alerts do not. Re-aggregating every record and
        // re-evaluating every threshold because a quota poll found the same
        // percentage it found a minute ago is work that cannot change its own
        // answer, and it would run all day.
        if event.data_changed {
            menubar::refresh(&self.app);
            alerts::evaluate_and_notify(&self.app);
        }
    }
}

/// Closing a window means "put it away", not "destroy it" — both windows have
/// to survive being closed so the other one can bring them back.
///
/// The one case that still exits is the one that always did: closing the
/// dashboard when the compact window is not on screen.
///
/// Focus is also where a catch-up trigger is submitted, because a window
/// coming forward is the moment stale numbers would be most noticeable. It
/// submits; it does not scan.
fn install_window_behavior(app: &tauri::AppHandle) {
    if let Some(main) = app.get_webview_window(mini::MAIN_LABEL) {
        let handle = app.clone();
        main.on_window_event(move |event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if mini::is_mini_visible(&handle) {
                    api.prevent_close();
                    if let Some(window) = handle.get_webview_window(mini::MAIN_LABEL) {
                        let _ = window.hide();
                    }
                }
            }
            tauri::WindowEvent::Focused(true) => catch_up(&handle),
            _ => {}
        });
    }

    if let Some(mini_window) = app.get_webview_window(mini::MINI_LABEL) {
        let handle = app.clone();
        mini_window.on_window_event(move |event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                mini::show_dashboard(&handle);
            }
            tauri::WindowEvent::Focused(true) => catch_up(&handle),
            _ => {}
        });
    }
}

fn catch_up(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<AppState>() {
        if let Some(refresh) = state.refresh() {
            refresh.submit(RefreshTrigger::WindowFocus);
        }
    }
}
