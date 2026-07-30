//! Storage boundary.
//!
//! Split in two on purpose. [`UsageReader`] is what the rest of the
//! application gets: commands, the menu bar, alerts, and query paths can look
//! but not touch. [`UsageWriter`] is held by exactly one production caller,
//! `crate::refresh`, and it is the only way data changes.
//!
//! That split is the type-level half of the single-owner rule. A future
//! command cannot accidentally become a second refresh path, because the
//! capability to write is simply not in the handle it is given.

pub mod memory;
pub mod sqlite;
pub mod summarize;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{
    RecentQuery, ReplaceScope, RepositoryRevision, SourceApp, SourceCheckpoint, SourceSyncHealth,
    SummaryQuery, UsageQuota, UsageRecord, UsageSummary,
};

pub use memory::InMemoryUsageRepository;
pub use sqlite::SqliteUsageRepository;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepositoryError {
    /// A thread panicked while holding the store's lock.
    #[error("the usage store is unavailable")]
    Unavailable,
    /// The backend failed for its own reasons — a disk error, a failed
    /// migration, a rolled-back transaction.
    #[error("storage error: {0}")]
    Backend(String),
}

/// What one committed transaction changed.
///
/// The four counts are reported separately because they answer different
/// questions: `inserted` is new activity, `updated` is a correction landing,
/// `unchanged` is a replay that cost nothing, and `deleted` is a window
/// replacement dropping an event the source no longer reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitCounts {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub deleted: usize,
}

impl CommitCounts {
    /// Whether anything visible actually moved. A transaction that only
    /// refreshed health metadata must not make the interface re-render.
    pub fn changed_records(&self) -> bool {
        self.inserted > 0 || self.updated > 0 || self.deleted > 0
    }

    pub fn add(&mut self, other: CommitCounts) {
        self.inserted += other.inserted;
        self.updated += other.updated;
        self.unchanged += other.unchanged;
        self.deleted += other.deleted;
    }
}

/// The result of a commit: what changed, and the revision it changed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitOutcome {
    pub revision: RepositoryRevision,
    pub counts: CommitCounts,
    /// False when nothing at all moved, in which case the revision did not
    /// advance either.
    pub advanced: bool,
    /// True when records or quotas moved, as opposed to only freshness.
    ///
    /// The distinction is what keeps a quiet minute quiet. A quota poll that
    /// finds the same numbers still advances "app synced … ago", which the
    /// interface should show — but recomputing token totals and re-evaluating
    /// alerts for it would be work with no possible result.
    pub data_changed: bool,
}

/// How an attempt should be recorded, independently of any data it carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthOutcome {
    /// A job started. Advances the attempt time and nothing else.
    Started,
    /// The attempt succeeded. Advances both freshness clocks.
    Succeeded {
        source_observed_at: Option<DateTime<Utc>>,
        awaiting_upstream: bool,
    },
    /// The attempt failed. Records the reason and the attempt time, and leaves
    /// the last-known-good values and their timestamps alone.
    Failed { offline: bool, error: String },
}

/// One source's atomic unit of change.
///
/// Everything in here commits together or not at all: records, the checkpoint
/// that says where to resume, the quota snapshot, and the health row. A
/// half-applied batch would leave a checkpoint claiming data that is not
/// stored, which is the one failure mode incremental reading cannot survive.
#[derive(Debug, Clone)]
pub struct SourceTransaction {
    pub source_app: SourceApp,
    pub adapter_id: String,
    pub records: Vec<UsageRecord>,
    /// Windows this batch replaces wholesale. Applied before the records, so
    /// the batch's own rows survive.
    pub replace_scopes: Vec<ReplaceScope>,
    pub checkpoints: Vec<SourceCheckpoint>,
    /// Quota snapshots to merge. Older observations are rejected by the
    /// backend, so a slow job cannot undo a fast one.
    pub quotas: Vec<UsageQuota>,
    /// Drop this source's stored windows that the batch did not re-report.
    /// Only ever set by a successful, authoritative quota read — a failed one
    /// must leave the last-good values in place.
    pub replace_quotas: bool,
    pub health: HealthOutcome,
    pub committed_at: DateTime<Utc>,
}

impl SourceTransaction {
    /// An empty transaction for one source, ready to be filled in.
    pub fn new(
        source_app: SourceApp,
        adapter_id: impl Into<String>,
        committed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            source_app,
            adapter_id: adapter_id.into(),
            records: Vec::new(),
            replace_scopes: Vec::new(),
            checkpoints: Vec::new(),
            quotas: Vec::new(),
            replace_quotas: false,
            health: HealthOutcome::Started,
            committed_at,
        }
    }

    /// A transaction that records an outcome and changes no data. Used for
    /// failures, which must never disturb last-known-good values.
    pub fn health_only(
        source_app: SourceApp,
        adapter_id: impl Into<String>,
        health: HealthOutcome,
        committed_at: DateTime<Utc>,
    ) -> Self {
        Self {
            health,
            ..Self::new(source_app, adapter_id, committed_at)
        }
    }
}

/// Everything one screen needs, read at one revision.
///
/// Fetched as a unit so the interface can never show a total from one revision
/// beside a quota from another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardSnapshot {
    pub revision: RepositoryRevision,
    /// Unfiltered, so proportions stay comparable while a filter is active.
    pub overview: UsageSummary,
    /// Reflects the requested filter.
    pub summary: UsageSummary,
    pub recent: Vec<UsageRecord>,
    pub record_count: usize,
    pub quotas: Vec<UsageQuota>,
    pub health: Vec<SourceSyncHealth>,
}

/// Read-only access. Everything but the coordinator gets this.
pub trait UsageReader: Send + Sync {
    fn summary(&self, query: &SummaryQuery) -> Result<UsageSummary, RepositoryError>;

    fn recent(&self, query: &RecentQuery) -> Result<Vec<UsageRecord>, RepositoryError>;

    fn count(&self) -> Result<usize, RepositoryError>;

    fn quotas(&self) -> Result<Vec<UsageQuota>, RepositoryError>;

    fn health(&self) -> Result<Vec<SourceSyncHealth>, RepositoryError>;

    fn revision(&self) -> Result<RepositoryRevision, RepositoryError>;

    fn checkpoints(&self, adapter_id: &str) -> Result<Vec<SourceCheckpoint>, RepositoryError>;

    /// One consistent read of everything a window renders.
    fn snapshot(
        &self,
        overview: &SummaryQuery,
        recent: &RecentQuery,
    ) -> Result<DashboardSnapshot, RepositoryError>;
}

/// Mutating access. Held by `crate::refresh` and nothing else in production.
pub trait UsageWriter: UsageReader {
    /// Apply one source's batch atomically and advance the revision.
    ///
    /// Failure changes neither data nor revision.
    fn commit(&self, transaction: SourceTransaction) -> Result<CommitOutcome, RepositoryError>;
}

/// Default snapshot assembly, shared by the backends.
pub(crate) fn assemble_snapshot(
    reader: &dyn UsageReader,
    overview_query: &SummaryQuery,
    recent_query: &RecentQuery,
) -> Result<DashboardSnapshot, RepositoryError> {
    Ok(DashboardSnapshot {
        revision: reader.revision()?,
        overview: reader.summary(overview_query)?,
        summary: reader.summary(&recent_query.filter)?,
        recent: reader.recent(recent_query)?,
        record_count: reader.count()?,
        quotas: reader.quotas()?,
        health: reader.health()?,
    })
}

/// The key a quota snapshot merges on: source, pool, window length.
///
/// A source can meter several windows at once and they are not
/// interchangeable, so all three parts matter.
pub(crate) fn quota_key(quota: &UsageQuota) -> (SourceApp, Option<String>, u32) {
    (
        quota.source_app,
        quota.label.clone(),
        quota.window_minutes,
    )
}

/// Merge one incoming quota snapshot into a stored set.
///
/// A snapshot is applied only when the *source* observed it later than the
/// stored one: wall-clock arrival order says nothing about which reading is
/// newer, and an overlapping slow job must not undo a fast one.
pub(crate) fn merge_quota(stored: &mut Vec<UsageQuota>, incoming: UsageQuota) -> bool {
    match stored
        .iter_mut()
        .find(|kept| quota_key(kept) == quota_key(&incoming))
    {
        Some(kept) => {
            if incoming.observed_at > kept.observed_at {
                *kept = incoming;
                true
            } else {
                false
            }
        }
        None => {
            stored.push(incoming);
            true
        }
    }
}

/// Whether two health rows differ in a way anything on screen depends on.
///
/// `last_attempt_at` is excluded on purpose. A quota poll that finds nothing
/// new still moves it, and if that counted as a change the interface would
/// refetch and the menu bar would repaint every minute for no reason. What
/// matters is the state, the two freshness clocks, and the error.
pub(crate) fn materially_differs(left: &SourceSyncHealth, right: &SourceSyncHealth) -> bool {
    left.state != right.state
        || left.app_synced_at != right.app_synced_at
        || left.source_observed_at != right.source_observed_at
        || left.last_error != right.last_error
        || left.awaiting_upstream != right.awaiting_upstream
}

/// Apply a health outcome to a stored row, in place.
pub(crate) fn apply_health(
    health: &mut SourceSyncHealth,
    outcome: &HealthOutcome,
    at: DateTime<Utc>,
) {
    use crate::domain::SyncState;

    health.last_attempt_at = Some(at);
    match outcome {
        HealthOutcome::Started => health.state = SyncState::Syncing,
        HealthOutcome::Succeeded {
            source_observed_at,
            awaiting_upstream,
        } => {
            health.state = SyncState::Current;
            health.app_synced_at = Some(at);
            // A source that reports no observation time of its own is only as
            // fresh as the read that found it.
            health.source_observed_at = source_observed_at.or(Some(at));
            health.last_error = None;
            health.awaiting_upstream = *awaiting_upstream;
        }
        HealthOutcome::Failed { offline, error } => {
            health.state = if *offline {
                SyncState::Offline
            } else {
                SyncState::Error
            };
            health.last_error = Some(error.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SyncState;
    use chrono::TimeZone;

    fn quota(window: u32, used: u16, observed: i64) -> UsageQuota {
        UsageQuota {
            source_app: SourceApp::Codex,
            label: None,
            window_minutes: window,
            used_percent_tenths: used,
            resets_at: None,
            observed_at: Utc.timestamp_opt(observed, 0).unwrap(),
        }
    }

    #[test]
    fn a_newer_source_observation_replaces_an_older_one() {
        let mut stored = vec![quota(10_080, 100, 1_000)];
        assert!(merge_quota(&mut stored, quota(10_080, 200, 2_000)));
        assert_eq!(stored[0].used_percent_tenths, 200);
    }

    #[test]
    fn an_older_overlapping_task_cannot_replace_a_newer_snapshot() {
        let mut stored = vec![quota(10_080, 200, 2_000)];
        assert!(!merge_quota(&mut stored, quota(10_080, 100, 1_000)));
        assert_eq!(stored[0].used_percent_tenths, 200);
    }

    #[test]
    fn windows_of_different_lengths_are_separate_pools() {
        let mut stored = vec![quota(10_080, 200, 2_000)];
        assert!(merge_quota(&mut stored, quota(300, 50, 1_000)));
        assert_eq!(stored.len(), 2);
    }

    #[test]
    fn a_failure_keeps_the_freshness_it_had() {
        let now = Utc.timestamp_opt(5_000, 0).unwrap();
        let mut health = SourceSyncHealth::unknown(SourceApp::Cursor);
        apply_health(
            &mut health,
            &HealthOutcome::Succeeded {
                source_observed_at: Some(Utc.timestamp_opt(4_000, 0).unwrap()),
                awaiting_upstream: false,
            },
            now,
        );
        let synced = health.app_synced_at;

        apply_health(
            &mut health,
            &HealthOutcome::Failed {
                offline: true,
                error: "no network".to_string(),
            },
            Utc.timestamp_opt(6_000, 0).unwrap(),
        );

        assert_eq!(health.state, SyncState::Offline);
        assert_eq!(health.app_synced_at, synced);
        assert_eq!(health.last_error.as_deref(), Some("no network"));
    }

    #[test]
    fn health_only_transactions_carry_no_data() {
        let transaction = SourceTransaction::health_only(
            SourceApp::Cursor,
            "cursor",
            HealthOutcome::Failed {
                offline: true,
                error: "no network".to_string(),
            },
            Utc::now(),
        );
        assert!(transaction.records.is_empty());
        assert!(transaction.quotas.is_empty());
        assert!(!transaction.replace_quotas);
    }
}
