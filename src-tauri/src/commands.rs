//! Tauri commands — the only surface React can reach.
//!
//! Commands stay thin: accept a request, delegate, convert failures into
//! [`AppError`]. No aggregation or parsing logic belongs here.

use tauri::{Emitter, State};

use crate::adapters::{self, AdapterInfo};
use crate::adapters::cursor::CursorConnectionStatus;
use crate::domain::{
    AppOptions, RecentQuery, SummaryQuery, ThresholdAlert, UsageQuota, UsageRecord, UsageSummary,
    HANDOFF_PROMPT,
};
use crate::error::AppError;
use crate::prefs;
use crate::refresh::RefreshReport;
use crate::state::AppState;

/// Totals and breakdowns for the dashboard.
#[tauri::command]
pub fn usage_summary(
    state: State<'_, AppState>,
    query: Option<SummaryQuery>,
) -> Result<UsageSummary, AppError> {
    let query = query.unwrap_or_default();
    validate(&query)?;
    Ok(state.repository().summary(&query)?)
}

/// The most recent usage records, newest first.
#[tauri::command]
pub fn recent_usage(
    state: State<'_, AppState>,
    query: Option<RecentQuery>,
) -> Result<Vec<UsageRecord>, AppError> {
    let query = query.unwrap_or_default();
    validate(&query.filter)?;
    Ok(state.repository().recent(&query)?)
}

/// How many records the store currently holds. The dashboard uses this to tell
/// an empty store apart from a filter that matched nothing.
#[tauri::command]
pub fn usage_record_count(state: State<'_, AppState>) -> Result<usize, AppError> {
    Ok(state.repository().count()?)
}

/// The sources this build knows about and whether each can read logs yet.
#[tauri::command]
pub fn usage_sources() -> Vec<AdapterInfo> {
    adapters::adapter_infos()
}

/// The freshest quota snapshot per source, when a source reports one.
/// Empty until the first refresh completes.
#[tauri::command]
pub fn usage_quota(state: State<'_, AppState>) -> Vec<UsageQuota> {
    state.quotas()
}

/// Rescan every source's logs and import whatever is new. Per-source failures
/// are inside the report; this only errors when the store itself is unusable.
#[tauri::command]
pub fn refresh_logs(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<RefreshReport, AppError> {
    let report = crate::refresh::refresh_all(state.repository());
    state.set_quotas(report.quotas.clone());
    crate::menubar::refresh(&app);
    crate::alerts::evaluate_and_notify(&app);
    Ok(report)
}

/// Current user options (thresholds, …).
#[tauri::command]
pub fn get_options(state: State<'_, AppState>) -> AppOptions {
    state.options()
}

/// Validate, persist, and replace user options.
#[tauri::command]
pub fn set_options(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    options: AppOptions,
) -> Result<AppOptions, AppError> {
    let options = options.normalized();
    options.validate().map_err(|error| AppError::invalid_request(error.to_string()))?;
    let path = state.options_path().ok_or_else(|| {
        AppError::new(
            crate::error::ErrorCode::Storage,
            "options path is not ready yet",
        )
    })?;
    prefs::save_options(&path, &options)?;
    state.replace_options(options.clone());
    crate::alerts::evaluate_and_notify(&app);
    Ok(options)
}

/// Whether an authenticated Cursor dashboard session is stored in Keychain.
#[tauri::command]
pub fn cursor_connection_status() -> Result<CursorConnectionStatus, AppError> {
    Ok(crate::adapters::cursor::cursor_connection_status()?)
}

/// Validate and save a Cursor dashboard session, then rebuild usage from the
/// authoritative remote events instead of incomplete local bubbles.
#[tauri::command]
pub fn connect_cursor_dashboard(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    session_token: String,
) -> Result<CursorConnectionStatus, AppError> {
    let status = crate::adapters::cursor::connect_cursor_dashboard(&session_token)?;
    rebuild_after_cursor_connection_change(&state, &app)?;
    Ok(status)
}

/// Remove the dashboard session from Keychain and return to local bubble data.
#[tauri::command]
pub fn disconnect_cursor_dashboard(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
) -> Result<CursorConnectionStatus, AppError> {
    let status = crate::adapters::cursor::disconnect_cursor_dashboard()?;
    rebuild_after_cursor_connection_change(&state, &app)?;
    Ok(status)
}

fn rebuild_after_cursor_connection_change(
    state: &AppState,
    app: &tauri::AppHandle,
) -> Result<(), AppError> {
    state.repository().clear()?;
    let report = crate::refresh::refresh_all(state.repository());
    state.set_quotas(report.quotas.clone());
    crate::menubar::refresh(app);
    crate::alerts::evaluate_and_notify(app);
    let _ = app.emit("usage-imported", &report);
    Ok(())
}

/// Active threshold alerts (non-snoozed crossings).
#[tauri::command]
pub fn active_alerts(state: State<'_, AppState>) -> Vec<ThresholdAlert> {
    crate::alerts::active_alerts(&state.options(), &state.quotas(), state.alerts())
}

/// Snooze an alert until its window resets (Continue).
#[tauri::command]
pub fn snooze_alert(
    state: State<'_, AppState>,
    app: tauri::AppHandle,
    dedupe_key: String,
) -> Result<Vec<ThresholdAlert>, AppError> {
    if dedupe_key.trim().is_empty() {
        return Err(AppError::invalid_request("dedupe key is required"));
    }
    state.alerts().snooze(dedupe_key);
    crate::alerts::evaluate_and_notify(&app);
    Ok(crate::alerts::active_alerts(
        &state.options(),
        &state.quotas(),
        state.alerts(),
    ))
}

/// The prompt to paste into the running agent when creating a HANDOFF.md.
#[tauri::command]
pub fn handoff_prompt() -> &'static str {
    HANDOFF_PROMPT
}

/// Fire a sample OS notification so the user can preview the alert look.
#[tauri::command]
pub fn test_notification(app: tauri::AppHandle) -> Result<(), AppError> {
    crate::alerts::send_test_notification(&app)
}

fn validate(query: &SummaryQuery) -> Result<(), AppError> {
    if let (Some(from), Some(until)) = (query.from, query.until) {
        if until <= from {
            return Err(AppError::invalid_request(
                "the end of the date range must be after its start",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SourceApp;
    use chrono::{TimeZone, Utc};

    #[test]
    fn rejects_an_inverted_date_range() {
        let query = SummaryQuery {
            from: Some(Utc.with_ymd_and_hms(2026, 7, 29, 0, 0, 0).unwrap()),
            until: Some(Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap()),
            ..SummaryQuery::default()
        };
        assert!(validate(&query).is_err());
    }

    #[test]
    fn accepts_an_open_ended_range() {
        let query = SummaryQuery {
            from: Some(Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap()),
            ..SummaryQuery::default()
        };
        assert!(validate(&query).is_ok());
    }

    #[test]
    fn reports_one_source_per_supported_application() {
        let sources = usage_sources();
        assert_eq!(sources.len(), SourceApp::ALL.len());
    }
}
