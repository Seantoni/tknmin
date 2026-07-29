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
    pub resets_at: DateTime<Utc>,
    /// When the source wrote this snapshot — quota goes stale quickly, so the
    /// interface shows the age rather than pretending it is live.
    pub observed_at: DateTime<Utc>,
}

impl UsageQuota {
    /// Tenths of a percent remaining, for "7% left this week".
    pub fn remaining_percent_tenths(&self) -> u16 {
        1000u16.saturating_sub(self.used_percent_tenths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remaining_never_underflows() {
        let quota = UsageQuota {
            source_app: SourceApp::Codex,
            label: None,
            window_minutes: 10080,
            used_percent_tenths: 1042,
            resets_at: Utc::now(),
            observed_at: Utc::now(),
        };
        assert_eq!(quota.remaining_percent_tenths(), 0);
    }
}
