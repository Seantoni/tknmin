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
    fn a_window_that_has_not_started_still_describes_the_present() {
        let now = Utc::now();
        assert!(quota(0, None).is_current_at(now));
        assert!(quota(500, Some(now + chrono::Duration::hours(1))).is_current_at(now));
        assert!(!quota(500, Some(now - chrono::Duration::minutes(1))).is_current_at(now));
    }
}
