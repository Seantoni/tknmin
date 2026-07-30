//! Account quota snapshots.
//!
//! A quota is not a locally reconstructed token total: it is the source's own
//! statement of how much of an allowance window has been consumed at a moment
//! in time. It may come from a local cache/log (Claude, Codex) or the source's
//! authenticated account endpoint (Cursor); the freshest snapshot wins.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::source::SourceApp;

/// How full one allowance window was, as the source last reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageQuota {
    pub source_app: SourceApp,
    /// A source-specific allowance pool, when several share one reset window
    /// (for example Cursor Models versus Other Models).
    #[serde(default)]
    pub label: Option<String>,
    /// The window's length in minutes; 10080 is a week.
    pub window_minutes: u32,
    /// Tenths of a percent consumed, so 930 means 93.0%. Kept as an integer
    /// for the same reason money never becomes a float.
    pub used_percent_tenths: u16,
    /// When the window rolls over, or `None` when no window is running.
    ///
    /// A rolling window only exists once something starts it: Claude reports a
    /// 5-hour session as zero used with no reset until the first request of a
    /// new session. That is a real, current statement — the whole session is
    /// available — so it is kept, rather than dropped for lacking a time or
    /// given an invented one five hours from now.
    #[serde(default)]
    pub resets_at: Option<DateTime<Utc>>,
    /// When the source wrote this snapshot — quota goes stale quickly, so the
    /// interface shows the age rather than pretending it is live.
    pub observed_at: DateTime<Utc>,
}

/// How far apart two `resets_at` may sit and still name the same window.
///
/// Sources restate the same nominal reset with a little jitter — six
/// consecutive readings of one Claude Code session window carried six distinct
/// millisecond values spread across 1.5 seconds. Compared exactly, no two
/// readings of a window ever match, and everything keyed on window identity
/// silently stops working: samples cannot be differenced, so no pace and no
/// projection is ever measured.
///
/// A genuinely new instance moves the reset by the window's whole length —
/// five hours at the shortest in use here — so any tolerance far below that
/// separates the two cases without ambiguity.
const WINDOW_INSTANCE_TOLERANCE: chrono::TimeDelta = chrono::TimeDelta::minutes(2);

/// Whether two readings describe the same window *instance*.
///
/// Identity matters because a delta taken across a reset spans a drop to zero:
/// differencing two instances invents a large negative pace, so callers only
/// ever difference within one.
pub fn same_window_instance(left: Option<DateTime<Utc>>, right: Option<DateTime<Utc>>) -> bool {
    match (left, right) {
        // Neither window is running: the same (absent) instance.
        (None, None) => true,
        (Some(left), Some(right)) => (left - right).abs() <= WINDOW_INSTANCE_TOLERANCE,
        // One is running and the other is not — a reset happened between them.
        _ => false,
    }
}

impl UsageQuota {
    /// Tenths of a percent remaining, for "7% left this week".
    pub fn remaining_percent_tenths(&self) -> u16 {
        1000u16.saturating_sub(self.used_percent_tenths)
    }

    /// Whether this window still describes the present.
    ///
    /// A window whose reset has passed does not: its percentage belongs to a
    /// window that no longer exists. A window that has not started does, and
    /// says the allowance is untouched.
    pub fn is_current_at(&self, now: DateTime<Utc>) -> bool {
        self.resets_at.is_none_or(|resets_at| resets_at > now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota(used_percent_tenths: u16, resets_at: Option<DateTime<Utc>>) -> UsageQuota {
        UsageQuota {
            source_app: SourceApp::Codex,
            label: None,
            window_minutes: 10080,
            used_percent_tenths,
            resets_at,
            observed_at: Utc::now(),
        }
    }

    #[test]
    fn remaining_never_underflows() {
        assert_eq!(quota(1042, Some(Utc::now())).remaining_percent_tenths(), 0);
    }

    #[test]
    fn jitter_in_a_restated_reset_does_not_invent_a_new_window() {
        // The six millisecond values one real session window was reported
        // with, across 41 minutes. Compared exactly, no two of them match.
        let at = |millis: i64| DateTime::from_timestamp_millis(millis);
        let readings = [
            1_785_463_799_920i64,
            1_785_463_799_746,
            1_785_463_800_578,
            1_785_463_799_087,
            1_785_463_799_091,
            1_785_463_799_513,
        ];
        for pair in readings.windows(2) {
            assert_ne!(pair[0], pair[1], "the fixture should hold distinct values");
            assert!(
                same_window_instance(at(pair[0]), at(pair[1])),
                "{} and {} are the same window",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn a_real_reset_is_still_a_different_window() {
        let now = Utc::now();
        // The shortest window in use rolls five hours on; nothing near the
        // tolerance can be mistaken for it.
        assert!(!same_window_instance(
            Some(now),
            Some(now + chrono::Duration::hours(5))
        ));
        assert!(!same_window_instance(
            Some(now),
            Some(now + chrono::Duration::minutes(3))
        ));
        // A window starting or ending is a change of instance either way.
        assert!(!same_window_instance(Some(now), None));
        assert!(!same_window_instance(None, Some(now)));
        assert!(same_window_instance(None, None));
    }

    #[test]
    fn a_window_that_has_not_started_still_describes_the_present() {
        let now = Utc::now();
        assert!(quota(0, None).is_current_at(now));
        assert!(quota(500, Some(now + chrono::Duration::hours(1))).is_current_at(now));
        assert!(!quota(500, Some(now - chrono::Duration::minutes(1))).is_current_at(now));
    }
}
