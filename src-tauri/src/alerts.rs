//! Deliver threshold alerts: evaluate → dedupe → notify → emit.
//!
//! The domain decides *whether* an alert exists. This module decides whether
//! it has already been shown, sends the macOS notification, and tells the
//! window so the in-app banner can offer Continue / Create HANDOFF.

use std::collections::HashSet;
use std::sync::Mutex;

use chrono::Utc;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_notification::{NotificationExt, PermissionState};

use crate::domain::{evaluate_alerts, AppOptions, ThresholdAlert, UsageQuota};
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;

/// Action identifiers on the OS notification. Handlers are not wired yet —
/// the buttons are shown so we can judge the look.
pub const ACTION_CREATE_HANDOFF: &str = "create_handoff";
pub const ACTION_CONTINUE: &str = "continue";

const ACTION_CREATE_HANDOFF_LABEL: &str = "Generate handoff MD file";
const ACTION_CONTINUE_LABEL: &str = "Continue";

/// In-process ledger of alerts already delivered this run, plus snoozes.
#[derive(Debug, Default)]
pub struct AlertLedger {
    /// Dedupe keys that already produced an OS notification this process.
    fired: Mutex<HashSet<String>>,
    /// Dedupe keys the user chose Continue on (until that window resets).
    snoozed: Mutex<HashSet<String>>,
}

impl AlertLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snooze(&self, dedupe_key: String) {
        if let Ok(mut snoozed) = self.snoozed.lock() {
            snoozed.insert(dedupe_key.clone());
        }
        if let Ok(mut fired) = self.fired.lock() {
            fired.insert(dedupe_key);
        }
    }

    pub fn snoozed_keys(&self) -> HashSet<String> {
        self.snoozed.lock().map(|set| set.clone()).unwrap_or_default()
    }

    /// Returns true the first time this key is marked fired.
    pub fn mark_fired(&self, dedupe_key: String) -> bool {
        self.fired
            .lock()
            .map(|mut set| set.insert(dedupe_key))
            .unwrap_or(false)
    }

    /// Drop ledger entries that no longer correspond to a crossing threshold
    /// (window reset, or usage recovered above the line).
    pub fn prune_to(&self, retain: &HashSet<String>) {
        if let Ok(mut fired) = self.fired.lock() {
            fired.retain(|key| retain.contains(key));
        }
        if let Ok(mut snoozed) = self.snoozed.lock() {
            snoozed.retain(|key| retain.contains(key));
        }
    }
}

/// Re-read options + quotas and notify for any new crossings.
pub fn evaluate_and_notify<R: Runtime>(app: &AppHandle<R>) {
    let Some(state) = app.try_state::<AppState>() else { return };
    let options = state.options();
    let quotas = state.quotas();
    evaluate_and_notify_with(app, &options, &quotas, state.alerts());
}

pub fn evaluate_and_notify_with<R: Runtime>(
    app: &AppHandle<R>,
    options: &AppOptions,
    quotas: &[UsageQuota],
    ledger: &AlertLedger,
) {
    let now = Utc::now();

    // Keys that would fire if nothing were snoozed — used to prune stale ledger rows.
    let open_keys: HashSet<String> = evaluate_alerts(options, quotas, now, &HashSet::new())
        .into_iter()
        .map(|alert| alert.dedupe_key)
        .collect();
    ledger.prune_to(&open_keys);

    let active = evaluate_alerts(options, quotas, now, &ledger.snoozed_keys());

    for alert in &active {
        if ledger.mark_fired(alert.dedupe_key.clone()) {
            ensure_permission(app);
            let _ = show_alert_notification(app, &alert.title(), &alert.body());
        }
    }

    let _ = app.emit("threshold-alerts", &active);
}

/// Active (non-snoozed) alerts right now — for the window on demand.
pub fn active_alerts(options: &AppOptions, quotas: &[UsageQuota], ledger: &AlertLedger) -> Vec<ThresholdAlert> {
    evaluate_alerts(options, quotas, Utc::now(), &ledger.snoozed_keys())
}

fn ensure_permission<R: Runtime>(app: &AppHandle<R>) {
    let notification = app.notification();
    match notification.permission_state() {
        Ok(PermissionState::Granted) => {}
        _ => {
            let _ = notification.request_permission();
        }
    }
}

/// Sample notification matching a real threshold alert, for Settings preview.
pub fn send_test_notification<R: Runtime>(app: &AppHandle<R>) -> Result<(), AppError> {
    ensure_permission(app);
    show_alert_notification(
        app,
        "Claude running low",
        "12% left this session · threshold 25%",
    )
}

/// macOS notification with the two alert actions visible (no handlers yet).
///
/// Uses `notify-rust` directly because Tauri's desktop notification plugin
/// does not forward action buttons on macOS.
fn show_alert_notification<R: Runtime>(
    app: &AppHandle<R>,
    title: &str,
    body: &str,
) -> Result<(), AppError> {
    let identifier = app.config().identifier.clone();

    #[cfg(target_os = "macos")]
    {
        let _ = notify_rust::set_application(if tauri::is_dev() {
            "com.apple.Terminal"
        } else {
            identifier.as_str()
        });
    }

    notify_rust::Notification::new()
        .summary(title)
        .body(body)
        .action(ACTION_CREATE_HANDOFF, ACTION_CREATE_HANDOFF_LABEL)
        .action(ACTION_CONTINUE, ACTION_CONTINUE_LABEL)
        .show()
        .map_err(|error| {
            AppError::new(
                ErrorCode::Storage,
                format!("could not show notification: {error}"),
            )
        })?;

    Ok(())
}
