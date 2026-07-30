//! Tauri commands — the only surface React can reach.
//!
//! Commands stay thin, and thinner than they used to be: they query committed
//! data or submit a trigger to the coordinator. None of them scans a source,
//! writes to the repository, refreshes the menu bar, evaluates alerts after a
//! scan, or emits a data-change event. Those all belong to `crate::refresh`,
//! which is what makes the interface's freshness independent of which command
//! happened to run last.

use tauri::State;

use crate::adapters::cursor::CursorConnectionStatus;
use crate::adapters::{self, AdapterInfo};
use crate::domain::{
    AppOptions, RecentQuery, SummaryQuery, ThresholdAlert, UsageQuota, UsageRecord, UsageSummary,
    HANDOFF_PROMPT,
};
use crate::error::AppError;
use crate::prefs;
use crate::refresh::RefreshTrigger;
use crate::repository::DashboardSnapshot;
use crate::state::AppState;

/// Everything one screen renders, read at a single revision.
///
/// One command rather than six, because six independent reads can interleave
/// with a commit and put a total from one revision beside a quota from
/// another. React fetches this and then ignores every event at or below the
/// revision it names.
#[tauri::command]
pub fn dashboard_snapshot(
    state: State<'_, AppState>,
    overview: Option<SummaryQuery>,
    recent: Option<RecentQuery>,
) -> Result<DashboardSnapshot, AppError> {
    let overview = overview.unwrap_or_default();
    let recent = recent.unwrap_or_default();
    validate(&overview)?;
    validate(&recent.filter)?;
    Ok(state.reader().snapshot(&overview, &recent)?)
}

/// Totals and breakdowns for one filter.
#[tauri::command]
pub fn usage_summary(
    state: State<'_, AppState>,
    query: Option<SummaryQuery>,
) -> Result<UsageSummary, AppError> {
    let query = query.unwrap_or_default();
    validate(&query)?;
    Ok(state.reader().summary(&query)?)
}

/// The most recent usage records, newest first.
#[tauri::command]
pub fn recent_usage(
    state: State<'_, AppState>,
    query: Option<RecentQuery>,
) -> Result<Vec<UsageRecord>, AppError> {
    let query = query.unwrap_or_default();
    validate(&query.filter)?;
    Ok(state.reader().recent(&query)?)
}

/// How many records the store currently holds. The dashboard uses this to tell
/// an empty store apart from a filter that matched nothing.
#[tauri::command]
pub fn usage_record_count(state: State<'_, AppState>) -> Result<usize, AppError> {
    Ok(state.reader().count()?)
}

/// Static facts about the sources this build supports.
///
/// Deliberately answers without touching a source: live status is committed
/// health, and it travels in the snapshot.
#[tauri::command]
pub fn source_capabilities() -> Vec<AdapterInfo> {
    adapters::adapter_infos()
}

/// The freshest committed allowance snapshot per source and window.
#[tauri::command]
pub fn usage_quota(state: State<'_, AppState>) -> Vec<UsageQuota> {
    state.quotas()
}

/// Ask the coordinator to catch up now.
///
/// Recovery and diagnostics only: every automatic path already reaches the
/// same queue, and removing this command entirely would not change whether
/// the application stays current.
#[tauri::command]
pub fn sync_now(state: State<'_, AppState>) {
    submit(&state, RefreshTrigger::Manual);
}

/// A window came to the front. A cheap catch-up trigger, never a scan.
#[tauri::command]
pub fn window_focused(state: State<'_, AppState>) {
    submit(&state, RefreshTrigger::WindowFocus);
}

/// Whether any source is being synchronized right now.
#[tauri::command]
pub fn is_syncing(state: State<'_, AppState>) -> bool {
    state.refresh().is_some_and(|refresh| refresh.is_busy())
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
    options
        .validate()
        .map_err(|error| AppError::invalid_request(error.to_string()))?;
    let path = state.options_path().ok_or_else(|| {
        AppError::new(
            crate::error::ErrorCode::Storage,
            "options path is not ready yet",
        )
    })?;
    prefs::save_options(&path, &options)?;
    state.replace_options(options.clone());
    crate::mini::set_always_on_top(&app, options.mini_always_on_top);
    // A moved threshold can cross on quotas that have not changed at all, so
    // this re-evaluates the committed ones. It reads no source.
    crate::alerts::evaluate_and_notify(&app);
    Ok(options)
}

/// Swap the dashboard for the compact always-on-top window.
#[tauri::command]
pub fn show_mini_window(app: tauri::AppHandle) {
    crate::mini::show_mini(&app);
}

/// Return from the compact window to the dashboard.
#[tauri::command]
pub fn show_dashboard_window(app: tauri::AppHandle) {
    crate::mini::show_dashboard(&app);
}

/// Whether an authenticated Cursor dashboard session is stored in Keychain.
#[tauri::command]
pub fn cursor_connection_status() -> Result<CursorConnectionStatus, AppError> {
    Ok(crate::adapters::cursor::cursor_connection_status()?)
}

/// Validate and save a Cursor dashboard session.
///
/// The rebuild that follows is not done here. Swapping which Cursor dataset is
/// authoritative is refresh work, so this submits the trigger and returns; the
/// interface learns the result from the next committed revision. That is also
/// what stops a half-swapped mix of local and dashboard rows from ever being
/// visible — the swap lands in one transaction or not at all.
#[tauri::command]
pub fn connect_cursor_dashboard(
    state: State<'_, AppState>,
    session_token: String,
) -> Result<CursorConnectionStatus, AppError> {
    let status = crate::adapters::cursor::connect_cursor_dashboard(&session_token)?;
    submit(&state, RefreshTrigger::CursorConnectionChanged);
    Ok(status)
}

/// Remove the dashboard session from Keychain and return to local bubble data.
#[tauri::command]
pub fn disconnect_cursor_dashboard(
    state: State<'_, AppState>,
) -> Result<CursorConnectionStatus, AppError> {
    let status = crate::adapters::cursor::disconnect_cursor_dashboard()?;
    submit(&state, RefreshTrigger::CursorConnectionChanged);
    Ok(status)
}

/// Active threshold alerts (non-snoozed crossings), against committed quotas.
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

/// The one way a command asks for work. Silent when no coordinator is
/// attached, which is only the case in tests.
fn submit(state: &AppState, trigger: RefreshTrigger) {
    if let Some(refresh) = state.refresh() {
        refresh.submit(trigger);
    }
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
        assert_eq!(source_capabilities().len(), SourceApp::ALL.len());
    }
}
