//! Deliver threshold alerts: evaluate → dedupe → notify → emit.
//!
//! The domain decides *whether* an alert exists. This module decides whether
//! it has already been shown, sends the macOS notification, and tells the
//! window so the in-app banner can offer Continue / Create HANDOFF.

use std::collections::HashSet;
use std::sync::Mutex;

use chrono::Utc;
use tauri::{AppHandle, Emitter, Manager, Runtime};
use tauri_plugin_clipboard_manager::ClipboardExt;

use crate::domain::{evaluate_alerts, AppOptions, ThresholdAlert, UsageQuota, HANDOFF_PROMPT};
use crate::error::{AppError, ErrorCode};
use crate::state::AppState;

/// Action identifiers on the OS notification.
pub const ACTION_CREATE_HANDOFF: &str = "create_handoff";
pub const ACTION_CONTINUE: &str = "continue";

const ACTION_CREATE_HANDOFF_LABEL: &str = "Copy handoff prompt";
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
        self.snoozed
            .lock()
            .map(|set| set.clone())
            .unwrap_or_default()
    }

    pub fn has_fired(&self, dedupe_key: &str) -> bool {
        self.fired
            .lock()
            .map(|set| set.contains(dedupe_key))
            .unwrap_or(true)
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
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
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
        if ledger.has_fired(&alert.dedupe_key) {
            continue;
        }
        // Only mark fired after macOS accepts delivery, so a denied permission
        // or transient failure can retry on the next poll.
        if ensure_notification_auth().is_err() {
            continue;
        }
        if show_alert_notification(
            app,
            &alert.title(),
            &alert.body(),
            Some(alert.dedupe_key.clone()),
        )
        .is_ok()
        {
            ledger.mark_fired(alert.dedupe_key.clone());
        }
    }

    let _ = app.emit("threshold-alerts", &active);
}

/// Active (non-snoozed) alerts right now — for the window on demand.
pub fn active_alerts(
    options: &AppOptions,
    quotas: &[UsageQuota],
    ledger: &AlertLedger,
) -> Vec<ThresholdAlert> {
    evaluate_alerts(options, quotas, Utc::now(), &ledger.snoozed_keys())
}

/// Ask macOS for notification permission (or confirm it is already granted).
///
/// Uses `UNUserNotificationCenter` via notify-rust's `preview-macos-un`
/// backend. The Tauri desktop notification plugin's permission APIs are
/// hard-coded to "granted" on macOS and must not be trusted.
pub fn ensure_notification_auth() -> Result<(), AppError> {
    #[cfg(target_os = "macos")]
    {
        notify_rust::check_bundle().map_err(|error| {
            AppError::new(
                ErrorCode::Storage,
                format!("notifications require a signed app bundle: {error}"),
            )
        })?;

        let granted = notify_rust::request_auth_blocking().map_err(|error| {
            AppError::new(
                ErrorCode::Storage,
                format!("could not request notification permission: {error}"),
            )
        })?;

        if !granted {
            return Err(AppError::new(
                ErrorCode::Storage,
                "notifications are denied — enable Tokens in System Settings → Notifications",
            ));
        }
    }

    Ok(())
}

/// Sample notification matching a real threshold alert, for Settings preview.
///
/// Returns only after macOS accepts the notification request (or an error).
pub fn send_test_notification<R: Runtime>(app: &AppHandle<R>) -> Result<(), AppError> {
    ensure_notification_auth()?;
    show_alert_notification(
        app,
        "Claude running low",
        "12% left this session · threshold 25%",
        None,
    )
}

/// macOS notification with working Continue / Create handoff actions.
///
/// Uses notify-rust's `UNUserNotificationCenter` backend (`preview-macos-un`)
/// so action buttons work and banners present while Tokens is foregrounded.
fn show_alert_notification<R: Runtime>(
    app: &AppHandle<R>,
    title: &str,
    body: &str,
    dedupe_key: Option<String>,
) -> Result<(), AppError> {
    let handle = notify_rust::Notification::new()
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

    let app = app.clone();
    std::thread::spawn(move || {
        handle.wait_for_action(|action| match action {
            ACTION_CREATE_HANDOFF => {
                show_main_window(&app);
                if app.clipboard().write_text(HANDOFF_PROMPT).is_ok() {
                    let _ = app.emit("threshold-handoff-copied", dedupe_key);
                }
            }
            ACTION_CONTINUE => {
                if let Some(dedupe_key) = dedupe_key {
                    if let Some(state) = app.try_state::<AppState>() {
                        state.alerts().snooze(dedupe_key);
                        evaluate_and_notify(&app);
                    }
                }
                show_main_window(&app);
            }
            "default" => show_main_window(&app),
            _ => {}
        });
    });

    Ok(())
}

/// Acting on a notification takes the user to the dashboard, since that is
/// where the banner and its handoff button live — even if the compact window
/// was the one on screen.
fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    crate::mini::show_dashboard(app);
}
