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

use crate::domain::{
    evaluate_alerts, live_alert_keys, AppOptions, ThresholdAlert, UsageQuota, WindowPace,
    HANDOFF_PROMPT,
};
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

    /// Drop ledger entries whose window instance is gone — the reset happened,
    /// so the key that identified it can never come back.
    ///
    /// Scoped to the instance, not to whether the threshold is crossing this
    /// second: a projected-exhaustion crossing comes and goes with the burn,
    /// and dropping a snooze during a lull would let the same notification
    /// fire again minutes later.
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
    let pace = state.pace();
    evaluate_and_notify_with(app, &options, &quotas, &pace, state.alerts());
}

pub fn evaluate_and_notify_with<R: Runtime>(
    app: &AppHandle<R>,
    options: &AppOptions,
    quotas: &[UsageQuota],
    pace: &[WindowPace],
    ledger: &AlertLedger,
) {
    let now = Utc::now();

    // Forget only what reset. A key is retained while its window instance is
    // live, whether or not it is crossing right now, so a snooze survives the
    // pace dipping below the line and back.
    ledger.prune_to(&live_alert_keys(options, quotas, now));

    let active = evaluate_alerts(options, quotas, pace, now, &ledger.snoozed_keys());

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
    pace: &[WindowPace],
    ledger: &AlertLedger,
) -> Vec<ThresholdAlert> {
    evaluate_alerts(options, quotas, pace, Utc::now(), &ledger.snoozed_keys())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SourceApp, SourceThreshold, ThresholdMetric};
    use chrono::{DateTime, TimeZone};

    fn options() -> AppOptions {
        AppOptions {
            thresholds: SourceApp::ALL
                .into_iter()
                .map(|source_app| SourceThreshold {
                    source_app,
                    enabled: true,
                    metric: ThresholdMetric::ProjectedExhaustion,
                    value: 30,
                })
                .collect(),
            ..AppOptions::defaults()
        }
    }

    fn quota(resets_at: DateTime<Utc>) -> UsageQuota {
        UsageQuota {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: 300,
            used_percent_tenths: 600,
            resets_at: Some(resets_at),
            observed_at: resets_at - chrono::Duration::hours(2),
        }
    }

    #[test]
    fn a_snooze_survives_the_pace_dipping_below_the_line() {
        // The user clicks Continue on a projected-exhaustion alert, then
        // pauses. The pace stops crossing, so no alert is open — but the
        // window has not reset, and the snooze must still be there when the
        // burst returns.
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();
        let quotas = vec![quota(now + chrono::Duration::hours(2))];
        let options = options();
        let ledger = AlertLedger::new();

        let key = live_alert_keys(&options, &quotas, now)
            .into_iter()
            .next()
            .expect("one live window, one enabled threshold");
        ledger.snooze(key.clone());

        // The lull: nothing is crossing, and the ledger is pruned anyway.
        ledger.prune_to(&live_alert_keys(&options, &quotas, now));
        assert!(
            ledger.snoozed_keys().contains(&key),
            "a snooze was discarded while its window was still running"
        );
        assert!(ledger.has_fired(&key), "the alert would notify a second time");
    }

    #[test]
    fn a_reset_clears_the_snooze() {
        // The other half of the contract: once the window resets, its key can
        // never come back, and Continue must not silence the next window too.
        let now = Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap();
        let options = options();
        let before = vec![quota(now + chrono::Duration::hours(2))];
        let ledger = AlertLedger::new();
        let key = live_alert_keys(&options, &before, now)
            .into_iter()
            .next()
            .expect("one live window, one enabled threshold");
        ledger.snooze(key.clone());

        let after = vec![quota(now + chrono::Duration::hours(7))];
        ledger.prune_to(&live_alert_keys(&options, &after, now));
        assert!(ledger.snoozed_keys().is_empty());
        assert!(!ledger.has_fired(&key));
    }
}
