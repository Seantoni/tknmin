//! Synchronization state: what the app knows about its own freshness.
//!
//! Two clocks, kept apart on purpose. `source_observed_at` is when the source
//! itself says its data was true; `app_synced_at` is when this app last read
//! it. Collapsing them would let a successful read of a stale file look live.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::source::SourceApp;

/// The repository revision. Monotonic, incremented once per committed
/// transaction, and the only number the interface uses to decide whether what
/// it is holding is current.
pub type RepositoryRevision = u64;

/// What a source's last synchronization attempt did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// Never attempted in this installation.
    Unknown,
    /// Watching for changes; the last attempt succeeded.
    Current,
    /// A job is running right now.
    Syncing,
    /// The last attempt succeeded, but longer ago than the source's data is
    /// expected to stay meaningful.
    Stale,
    /// The last attempt failed for a reason that looks like connectivity.
    Offline,
    /// The last attempt failed for another reason. `last_error` says which.
    Error,
}

/// One source's synchronization health, as committed to the repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSyncHealth {
    pub source_app: SourceApp,
    pub state: SyncState,
    /// When this app last completed a successful sync of the source.
    pub app_synced_at: Option<DateTime<Utc>>,
    /// When the source itself says its data was last true.
    pub source_observed_at: Option<DateTime<Utc>>,
    /// When this app last tried, successful or not.
    pub last_attempt_at: Option<DateTime<Utc>>,
    /// The last failure, already stripped of tokens and paths.
    pub last_error: Option<String>,
    /// Cursor publishes billing well after the activity that caused it. True
    /// once local activity has been seen but the authoritative window has not
    /// caught up, so the interface can say so rather than imply zero usage.
    #[serde(default)]
    pub awaiting_upstream: bool,
}

impl SourceSyncHealth {
    pub fn unknown(source_app: SourceApp) -> Self {
        Self {
            source_app,
            state: SyncState::Unknown,
            app_synced_at: None,
            source_observed_at: None,
            last_attempt_at: None,
            last_error: None,
            awaiting_upstream: false,
        }
    }
}

/// A source's resume point, persisted between runs.
///
/// The payload is opaque to everything but the adapter that wrote it: the
/// coordinator stores and returns it, and never interprets it. It must never
/// contain a credential — the coordinator persists this verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceCheckpoint {
    pub adapter_id: String,
    /// Identifies one unit within the source: a rollout file, a transcript, a
    /// server watermark. Unique per adapter.
    pub source_key: String,
    /// Adapter-defined JSON.
    pub payload: serde_json::Value,
}

/// A slice of one source's data that a transaction replaces wholesale.
///
/// Used where a source has no trustworthy immutable identifier and the only
/// way to apply a correction is to re-read a bounded window and swap it: any
/// stored record inside the window that the new batch did not produce is
/// deleted in the same transaction, so a corrected cost never coexists with
/// the obsolete one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplaceScope {
    pub source_app: SourceApp,
    /// Restricts the scope to records this adapter produced, so local and
    /// dashboard Cursor rows can never delete each other.
    pub adapter_id: String,
    pub from: DateTime<Utc>,
    pub until: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_never_attempted_source_claims_nothing() {
        let health = SourceSyncHealth::unknown(SourceApp::Cursor);
        assert_eq!(health.state, SyncState::Unknown);
        assert!(health.app_synced_at.is_none());
        assert!(health.source_observed_at.is_none());
    }
}
