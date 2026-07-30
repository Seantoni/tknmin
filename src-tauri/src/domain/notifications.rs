//! Threshold alerts — when an allowance crosses a user-set line.
//!
//! Pure evaluation lives here. Delivery (OS notification, in-app banner) and
//! the fired/snoozed ledger sit outside the domain so this module stays free
//! of I/O and Tauri.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::options::{AppOptions, SourceThreshold, ThresholdMetric};
use super::quota::UsageQuota;
use super::source::SourceApp;

/// What the user can do from an alert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertAction {
    /// Ask the current agent to write a HANDOFF.md brief.
    CreateHandoff,
    /// Snooze until this window resets.
    Continue,
}

/// One firing of a source threshold against a live quota window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThresholdAlert {
    pub source_app: SourceApp,
    pub window_minutes: u32,
    pub metric: ThresholdMetric,
    pub threshold_value: u32,
    /// Tenths of a percent remaining on the window that tripped the alert.
    pub remaining_percent_tenths: u16,
    pub minutes_until_reset: u32,
    pub resets_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    /// Stable identity for dedupe / snooze: source + window + metric + value + reset.
    pub dedupe_key: String,
}

impl ThresholdAlert {
    pub fn title(&self) -> String {
        format!(
            "{} running low",
            match self.source_app {
                SourceApp::ClaudeCode => "Claude",
                SourceApp::Cursor => "Cursor",
                SourceApp::Codex => "Codex",
            }
        )
    }

    pub fn body(&self) -> String {
        let window = describe_window(self.window_minutes);
        match self.metric {
            ThresholdMetric::RemainingPercent => {
                let left = format_tenths(self.remaining_percent_tenths);
                format!(
                    "{left}% left {window} · threshold {threshold}%",
                    threshold = self.threshold_value
                )
            }
            ThresholdMetric::MinutesUntilReset => {
                format!(
                    "resets in {minutes} min {window} · threshold {threshold} min",
                    minutes = self.minutes_until_reset,
                    threshold = self.threshold_value
                )
            }
        }
    }
}

/// The fixed instruction Claude (or another agent) should receive when the
/// user chooses Create HANDOFF. Kept here so Rust and React share one string.
pub const HANDOFF_PROMPT: &str = "Write `.handoff/HANDOFF.md` in this project using this structure:\n\n# Handoff\n- From: <this tool>\n- To: cursor\n- Why: allowance running low\n\n## Goal\n## Done so far\n## Current state\n## Next actions\n## Do not\n## Open questions\n\nSummarize from this conversation. Stop after writing the file.";

/// Compare live quotas to the user's thresholds. Returns only alerts that are
/// not already snoozed and whose window has not yet reset.
pub fn evaluate_alerts(
    options: &AppOptions,
    quotas: &[UsageQuota],
    now: DateTime<Utc>,
    snoozed_keys: &std::collections::HashSet<String>,
) -> Vec<ThresholdAlert> {
    let mut alerts = Vec::new();

    for threshold in &options.thresholds {
        if !threshold.enabled {
            continue;
        }
        let Some(quota) = relevant_quota(threshold, quotas, now) else {
            continue;
        };
        if !crosses_threshold(threshold, quota, now) {
            continue;
        }

        let Some(resets_at) = quota.resets_at else {
            continue;
        };
        let minutes_until_reset = minutes_until(resets_at, now);
        let alert = ThresholdAlert {
            source_app: quota.source_app,
            window_minutes: quota.window_minutes,
            metric: threshold.metric,
            threshold_value: threshold.value,
            remaining_percent_tenths: quota.remaining_percent_tenths(),
            minutes_until_reset,
            resets_at,
            observed_at: quota.observed_at,
            dedupe_key: dedupe_key(threshold, quota),
        };

        if snoozed_keys.contains(&alert.dedupe_key) {
            continue;
        }
        alerts.push(alert);
    }

    alerts.sort_by(|left, right| {
        left.source_app
            .cmp(&right.source_app)
            .then(left.window_minutes.cmp(&right.window_minutes))
    });
    alerts
}

fn relevant_quota<'a>(
    threshold: &SourceThreshold,
    quotas: &'a [UsageQuota],
    now: DateTime<Utc>,
) -> Option<&'a UsageQuota> {
    // A window that has not started cannot be warned about: nothing of it is
    // used, and there is no reset to count down to.
    let live: Vec<&UsageQuota> = quotas
        .iter()
        .filter(|quota| {
            quota.source_app == threshold.source_app
                && quota.resets_at.is_some_and(|resets_at| resets_at > now)
        })
        .collect();
    if live.is_empty() {
        return None;
    }

    match threshold.metric {
        ThresholdMetric::RemainingPercent => live
            .into_iter()
            .min_by_key(|quota| quota.remaining_percent_tenths()),
        ThresholdMetric::MinutesUntilReset => live.into_iter().min_by_key(|quota| quota.resets_at),
    }
}

fn crosses_threshold(threshold: &SourceThreshold, quota: &UsageQuota, now: DateTime<Utc>) -> bool {
    match threshold.metric {
        ThresholdMetric::RemainingPercent => {
            let remaining_whole = u32::from(quota.remaining_percent_tenths() / 10);
            remaining_whole <= threshold.value
        }
        ThresholdMetric::MinutesUntilReset => quota
            .resets_at
            .is_some_and(|resets_at| minutes_until(resets_at, now) <= threshold.value),
    }
}

fn minutes_until(resets_at: DateTime<Utc>, now: DateTime<Utc>) -> u32 {
    if resets_at <= now {
        return 0;
    }
    let secs = (resets_at - now).num_seconds().max(0) as u64;
    secs.div_ceil(60) as u32
}

fn dedupe_key(threshold: &SourceThreshold, quota: &UsageQuota) -> String {
    format!(
        "{}:{}:{:?}:{}:{}",
        threshold.source_app.as_str(),
        quota.window_minutes,
        threshold.metric,
        threshold.value,
        quota.resets_at.map_or(0, |resets_at| resets_at.timestamp())
    )
}

fn describe_window(window_minutes: u32) -> &'static str {
    match window_minutes {
        300 => "this session",
        10_080 => "this week",
        _ => "this window",
    }
}

fn format_tenths(tenths: u16) -> String {
    let whole = tenths / 10;
    let frac = tenths % 10;
    if frac == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{frac}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{SourceThreshold, ThresholdMetric};
    use chrono::TimeZone;

    fn quota(
        source: SourceApp,
        window: u32,
        used_tenths: u16,
        resets_at: DateTime<Utc>,
    ) -> UsageQuota {
        UsageQuota {
            source_app: source,
            label: None,
            window_minutes: window,
            used_percent_tenths: used_tenths,
            resets_at: Some(resets_at),
            observed_at: resets_at - chrono::Duration::minutes(5),
        }
    }

    #[test]
    fn percent_threshold_fires_when_remaining_at_or_below() {
        let now = Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap();
        let resets = now + chrono::Duration::hours(3);
        let options = AppOptions {
            thresholds: vec![
                SourceThreshold {
                    source_app: SourceApp::ClaudeCode,
                    enabled: true,
                    metric: ThresholdMetric::RemainingPercent,
                    value: 25,
                },
                SourceThreshold {
                    source_app: SourceApp::Cursor,
                    enabled: false,
                    metric: ThresholdMetric::RemainingPercent,
                    value: 25,
                },
                SourceThreshold {
                    source_app: SourceApp::Codex,
                    enabled: false,
                    metric: ThresholdMetric::RemainingPercent,
                    value: 25,
                },
            ],
            ..AppOptions::defaults()
        };
        let quotas = vec![quota(SourceApp::ClaudeCode, 300, 800, resets)]; // 20% left
        let alerts = evaluate_alerts(&options, &quotas, now, &Default::default());
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].remaining_percent_tenths, 200);
    }

    #[test]
    fn percent_threshold_stays_quiet_above_line() {
        let now = Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap();
        let resets = now + chrono::Duration::hours(3);
        let options = AppOptions {
            thresholds: SourceApp::ALL
                .into_iter()
                .map(|source_app| SourceThreshold {
                    source_app,
                    enabled: source_app == SourceApp::Codex,
                    metric: ThresholdMetric::RemainingPercent,
                    value: 25,
                })
                .collect(),
            ..AppOptions::defaults()
        };
        let quotas = vec![quota(SourceApp::Codex, 10_080, 500, resets)]; // 50% left
        assert!(evaluate_alerts(&options, &quotas, now, &Default::default()).is_empty());
    }

    #[test]
    fn minutes_threshold_fires_near_reset() {
        let now = Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap();
        let resets = now + chrono::Duration::minutes(40);
        let options = AppOptions {
            thresholds: SourceApp::ALL
                .into_iter()
                .map(|source_app| SourceThreshold {
                    source_app,
                    enabled: source_app == SourceApp::ClaudeCode,
                    metric: ThresholdMetric::MinutesUntilReset,
                    value: 60,
                })
                .collect(),
            ..AppOptions::defaults()
        };
        let quotas = vec![quota(SourceApp::ClaudeCode, 300, 100, resets)];
        let alerts = evaluate_alerts(&options, &quotas, now, &Default::default());
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].minutes_until_reset, 40);
    }

    #[test]
    fn snoozed_key_suppresses_alert() {
        let now = Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap();
        let resets = now + chrono::Duration::hours(1);
        let options = AppOptions {
            thresholds: SourceApp::ALL
                .into_iter()
                .map(|source_app| SourceThreshold {
                    source_app,
                    enabled: true,
                    metric: ThresholdMetric::RemainingPercent,
                    value: 50,
                })
                .collect(),
            ..AppOptions::defaults()
        };
        let quotas = vec![quota(SourceApp::Cursor, 10_080, 900, resets)];
        let open = evaluate_alerts(&options, &quotas, now, &Default::default());
        assert_eq!(open.len(), 1);
        let mut snoozed = std::collections::HashSet::new();
        snoozed.insert(open[0].dedupe_key.clone());
        assert!(evaluate_alerts(&options, &quotas, now, &snoozed).is_empty());
    }

    #[test]
    fn picks_tightest_window_for_percent() {
        let now = Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap();
        let options = AppOptions {
            thresholds: SourceApp::ALL
                .into_iter()
                .map(|source_app| SourceThreshold {
                    source_app,
                    enabled: source_app == SourceApp::ClaudeCode,
                    metric: ThresholdMetric::RemainingPercent,
                    value: 30,
                })
                .collect(),
            ..AppOptions::defaults()
        };
        let quotas = vec![
            quota(
                SourceApp::ClaudeCode,
                10_080,
                200,
                now + chrono::Duration::days(3),
            ), // 80% left
            quota(
                SourceApp::ClaudeCode,
                300,
                850,
                now + chrono::Duration::hours(2),
            ), // 15% left
        ];
        let alerts = evaluate_alerts(&options, &quotas, now, &Default::default());
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].window_minutes, 300);
    }
}
