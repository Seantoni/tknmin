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
    apply_projections, build_baseline, evaluate_pace, fit_calibrations, project_quotas,
    recent_token_rates, QuotaProjection, QuotaSample, RecentQuery, ReplaceScope,
    RepositoryRevision, SourceApp, SourceBaseline, SourceCheckpoint, SourceSyncHealth,
    SummaryQuery, ThreadUsageQuery, ThreadUsageReport, UsageQuota, UsageRecord, UsageSummary,
    WindowPace, BASELINE_DAYS, RATE_WINDOW_MINUTES,
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
    /// Per-thread usage under the same filter as `summary`, so a thread's row
    /// always agrees with the totals beside it.
    pub thread_usage: ThreadUsageReport,
    pub recent: Vec<UsageRecord>,
    pub record_count: usize,
    /// Exactly what the sources reported, never a derived value. The interface
    /// renders these, and shows any [`Self::projections`] entry beside its row
    /// as the inferred part.
    pub quotas: Vec<UsageQuota>,
    pub health: Vec<SourceSyncHealth>,
    /// One pace row per live allowance window, computed from the quotas in
    /// this same snapshot so a projection can never sit beside a quota from a
    /// different revision than the one that produced it.
    pub pace: Vec<WindowPace>,
    /// Confirmed readings carried forward to this snapshot's instant, for the
    /// windows where a trustworthy rate could be fitted. A window is absent
    /// when no rate was earned, when nothing was spent since the reading, or
    /// when the reading is too old to project from — in every one of those
    /// cases the confirmed number in [`Self::quotas`] stands alone.
    pub projections: Vec<QuotaProjection>,
}

/// Everything about *allowances* at one revision, and nothing about totals.
///
/// The same four fields the dashboard snapshot carries, read the same way at
/// one revision — but without the two summaries, the thread report, the record
/// list and the count that sit beside them there.
///
/// That difference is not small. Measured against a 50,000-record store, a
/// dashboard snapshot costs about 460ms, of which the allowance half is 30ms:
/// the rest is aggregation that deserialises every record's payload. The
/// compact window renders allowances only, and the menu bar less than that, so
/// asking either of them to pay the other 430ms put a third of a second of
/// work behind every refresh they could otherwise have done fifteen times
/// over.
///
/// Freshness is why that mattered rather than merely being wasteful: a window
/// that takes 460ms to redraw cannot follow a source that changes every
/// couple of seconds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RiskSnapshot {
    pub revision: RepositoryRevision,
    pub quotas: Vec<UsageQuota>,
    pub health: Vec<SourceSyncHealth>,
    pub pace: Vec<WindowPace>,
    pub projections: Vec<QuotaProjection>,
}

/// Read-only access. Everything but the coordinator gets this.
pub trait UsageReader: Send + Sync {
    fn summary(&self, query: &SummaryQuery) -> Result<UsageSummary, RepositoryError>;

    fn recent(&self, query: &RecentQuery) -> Result<Vec<UsageRecord>, RepositoryError>;

    /// Per-thread usage for one filter: what each conversation consumed.
    fn thread_usage(&self, query: &ThreadUsageQuery) -> Result<ThreadUsageReport, RepositoryError>;

    /// Every record at or after `since`, unfiltered and uncapped.
    ///
    /// Deliberately not [`UsageReader::recent`]. That one serves a list the
    /// user is looking at: it honours the dashboard's filter and stops at a few
    /// dozen rows. Risk must do neither. A baseline drawn through the active
    /// filter would make the projection change when the user clicks a source
    /// chip, and one drawn from the newest 50 records is not a month of history
    /// — it is a few minutes of it, which is also the wrong direction for a
    /// current burn, since capping the newest rows undercounts the busiest
    /// hours by exactly as much as they are busy.
    fn records_since(&self, since: DateTime<Utc>) -> Result<Vec<UsageRecord>, RepositoryError>;

    fn count(&self) -> Result<usize, RepositoryError>;

    fn quotas(&self) -> Result<Vec<UsageQuota>, RepositoryError>;

    fn health(&self) -> Result<Vec<SourceSyncHealth>, RepositoryError>;

    fn revision(&self) -> Result<RepositoryRevision, RepositoryError>;

    fn checkpoints(&self, adapter_id: &str) -> Result<Vec<SourceCheckpoint>, RepositoryError>;

    /// Historical quota observations at or after `since`, oldest first, for
    /// pace measurement. Returns nothing until the sample-capturing schema is
    /// in place; until then every pace falls back to its window-open rung.
    fn quota_samples(&self, _since: DateTime<Utc>) -> Result<Vec<QuotaSample>, RepositoryError> {
        Ok(Vec::new())
    }

    /// One consistent read of everything a window renders.
    fn snapshot(
        &self,
        overview: &SummaryQuery,
        recent: &RecentQuery,
    ) -> Result<DashboardSnapshot, RepositoryError>;

    /// One consistent read of the allowance half alone — see [`RiskSnapshot`]
    /// for why the two are separate reads rather than one.
    fn risk_snapshot(&self) -> Result<RiskSnapshot, RepositoryError>;

    /// Just the pace of every live window, for callers that need risk and
    /// nothing else — the alert lane runs this on every refresh tick and every
    /// notification action, and assembling a whole dashboard to read one field
    /// would put two summaries, a count and a record list on that path. Same
    /// inputs as the snapshot's, so the banner and the notification agree.
    fn pace(&self) -> Result<Vec<WindowPace>, RepositoryError>;
}

/// Mutating access. Held by `crate::refresh` and nothing else in production.
pub trait UsageWriter: UsageReader {
    /// Apply one source's batch atomically and advance the revision.
    ///
    /// Failure changes neither data nor revision.
    fn commit(&self, transaction: SourceTransaction) -> Result<CommitOutcome, RepositoryError>;

    /// Drop quota samples older than `before`. They exist only to measure a
    /// trailing pace, so once they fall outside every horizon in use they are
    /// weight. Called on the idle path, not on the commit path.
    fn prune_quota_samples(&self, _before: DateTime<Utc>) -> Result<usize, RepositoryError> {
        Ok(0)
    }
}

/// Default snapshot assembly, shared by the backends.
pub(crate) fn assemble_snapshot(
    reader: &dyn UsageReader,
    overview_query: &SummaryQuery,
    recent_query: &RecentQuery,
) -> Result<DashboardSnapshot, RepositoryError> {
    // Read the revision before any payload. If a commit lands during the
    // remaining reads, the snapshot carries the older revision and the
    // already-subscribed interface will refetch for the newer event. Reading
    // it later could stamp old quota/pace data with the new revision and make
    // the interface discard the one event that would repair it.
    // One instant for the whole assembly. Reading the clock again per
    // measurement would let a slow read date the burn, the baseline and the
    // projection differently, which is the same inconsistency the revision
    // rule above exists to prevent.
    let now = Utc::now();
    let revision = reader.revision()?;
    let quotas = reader.quotas()?;
    let (pace, projections) = assemble_risk(reader, &quotas, now)?;
    Ok(DashboardSnapshot {
        revision,
        overview: reader.summary(overview_query)?,
        summary: reader.summary(&recent_query.filter)?,
        thread_usage: reader.thread_usage(&ThreadUsageQuery {
            filter: recent_query.filter.clone(),
            limit: ThreadUsageQuery::DEFAULT_LIMIT,
        })?,
        recent: reader.recent(recent_query)?,
        record_count: reader.count()?,
        quotas,
        health: reader.health()?,
        pace,
        projections,
    })
}

/// Default allowance-only assembly, shared by the backends.
///
/// The same order and the same reasoning as [`assemble_snapshot`]: revision
/// first so a commit landing mid-read is refetched rather than skipped, and
/// one instant for the whole assembly.
pub(crate) fn assemble_risk_snapshot(
    reader: &dyn UsageReader,
) -> Result<RiskSnapshot, RepositoryError> {
    let now = Utc::now();
    let revision = reader.revision()?;
    let quotas = reader.quotas()?;
    let (pace, projections) = assemble_risk(reader, &quotas, now)?;
    Ok(RiskSnapshot {
        revision,
        quotas,
        health: reader.health()?,
        pace,
        projections,
    })
}

/// How far back samples are kept for pace measurement: the longest window in
/// use is a monthly billing cycle, plus margin.
///
/// Public because the coordinator prunes against it. Pruning to anything
/// shorter than this silently caps what the calibration in
/// [`crate::domain::projection`] can ever see, and a monthly window whose
/// history is trimmed to a week can never accumulate enough movement to be
/// fitted — the horizon and the prune have to be the same number.
pub const SAMPLE_HORIZON_DAYS: i64 = 62;

/// Every live window's pace, from quotas already read at this revision.
///
/// Shared by the snapshot and the pace-only path so the dashboard, the banner
/// and the OS notification cannot disagree about the same window.
pub(crate) fn assemble_pace(
    reader: &dyn UsageReader,
    quotas: &[UsageQuota],
    now: DateTime<Utc>,
) -> Result<Vec<WindowPace>, RepositoryError> {
    Ok(assemble_risk(reader, quotas, now)?.0)
}

/// Pace and the projections it was computed on, from one read of the history.
///
/// The two are produced together because they must agree: a banner saying
/// "runs out in 40 minutes" beside a percentage that does not reflect the same
/// burn is worse than either alone. The snapshot needs both; the pace-only
/// path takes the first and drops the second.
pub(crate) fn assemble_risk(
    reader: &dyn UsageReader,
    quotas: &[UsageQuota],
    now: DateTime<Utc>,
) -> Result<(Vec<WindowPace>, Vec<QuotaProjection>), RepositoryError> {
    // Until a backend stores samples this is empty and every pace rests on its
    // window-open rung alone.
    let samples = reader.quota_samples(now - chrono::Duration::days(SAMPLE_HORIZON_DAYS))?;
    // History for the baseline and the current burn: unfiltered and uncapped,
    // so neither depends on what the dashboard happens to be showing.
    let history = reader.records_since(now - chrono::Duration::days(i64::from(BASELINE_DAYS)))?;
    // The local timezone is unknown to a pure domain, so the offset is taken
    // from the host here at the boundary, in minutes because not every zone is
    // a whole hour from UTC. DST drift of an hour smears a slot boundary but
    // does not invert the pattern.
    let tz_offset_minutes = i64::from(chrono::Local::now().offset().local_minus_utc()) / 60;
    let baselines: Vec<SourceBaseline> = SourceApp::ALL
        .iter()
        .map(|source| build_baseline(*source, &history, tz_offset_minutes, now))
        .collect();
    let recent_rates = recent_token_rates(&history, RATE_WINDOW_MINUTES, now);

    // A source's allowance percentages are only as fresh as the source chose to
    // publish them — Claude Code refreshes its cache every several minutes while
    // active and not at all while idle — but the records are current to seconds.
    // Fitting the rate between the two from the stored samples lets the confirmed
    // reading be carried forward, so pace measures the burn as it stands rather
    // than as it stood at the last publication. Where no rate is earned this is a
    // no-op and the confirmed reading flows through untouched.
    let calibrations = fit_calibrations(quotas, &samples, &history);
    let projections = project_quotas(quotas, &calibrations, &history, now);
    let projected = apply_projections(quotas, &projections);

    Ok((
        evaluate_pace(
            &projected,
            &samples,
            &baselines,
            &recent_rates,
            tz_offset_minutes,
            now,
        ),
        projections,
    ))
}

/// The pace-only read path: quotas plus [`assemble_pace`].
pub(crate) fn assemble_pace_only(
    reader: &dyn UsageReader,
) -> Result<Vec<WindowPace>, RepositoryError> {
    let quotas = reader.quotas()?;
    assemble_pace(reader, &quotas, Utc::now())
}

/// The key a quota snapshot merges on: source, pool, window length.
///
/// A source can meter several windows at once and they are not
/// interchangeable, so all three parts matter.
pub(crate) fn quota_key(quota: &UsageQuota) -> (SourceApp, Option<String>, u32) {
    (quota.source_app, quota.label.clone(), quota.window_minutes)
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
            // The two clocks answer different questions and must not be
            // conflated: `app_synced_at` is when we last looked, and this is
            // when the source last spoke. A sync that finds nothing new moves
            // the first and must leave the second where it was — Codex writes
            // its rate limits only while it runs, so between sessions every
            // poll would otherwise re-date a day-old allowance to "just now"
            // and the tooltip would vouch for a number nothing had confirmed.
            //
            // Only a source that has never given an observation time falls
            // back to the read, so a first sync still has something to show.
            health.source_observed_at = source_observed_at
                .or(health.source_observed_at)
                .or(Some(at));
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
    use std::sync::atomic::{AtomicBool, Ordering};

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
    fn a_sync_that_finds_nothing_new_does_not_re_date_the_source() {
        // Codex writes its rate limits into a rollout file only while it runs,
        // so between sessions every poll succeeds and carries no observation.
        // Dating those to the read would have the tooltip vouch for a day-old
        // allowance as if the source had just restated it.
        let now = Utc.timestamp_opt(5_000, 0).unwrap();
        let observed = Utc.timestamp_opt(4_000, 0).unwrap();
        let mut health = SourceSyncHealth::unknown(SourceApp::Codex);
        apply_health(
            &mut health,
            &HealthOutcome::Succeeded {
                source_observed_at: Some(observed),
                awaiting_upstream: false,
            },
            now,
        );

        let later = Utc.timestamp_opt(90_000, 0).unwrap();
        apply_health(
            &mut health,
            &HealthOutcome::Succeeded {
                source_observed_at: None,
                awaiting_upstream: false,
            },
            later,
        );

        assert_eq!(health.state, SyncState::Current);
        assert_eq!(health.app_synced_at, Some(later), "we did look just now");
        assert_eq!(
            health.source_observed_at,
            Some(observed),
            "but the source has not spoken since"
        );
    }

    #[test]
    fn a_source_that_has_never_reported_is_dated_by_the_read_that_found_it() {
        let now = Utc.timestamp_opt(5_000, 0).unwrap();
        let mut health = SourceSyncHealth::unknown(SourceApp::Codex);
        apply_health(
            &mut health,
            &HealthOutcome::Succeeded {
                source_observed_at: None,
                awaiting_upstream: false,
            },
            now,
        );
        assert_eq!(health.source_observed_at, Some(now));
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

    /// The whole point of the projection, end to end: a source that stopped
    /// publishing minutes ago, records that did not, and a number that moves.
    ///
    /// Claude Code refreshes its allowance cache every several minutes and not
    /// at all while idle, so between refreshes the confirmed percentage is the
    /// last thing it said, not the truth. This asserts the snapshot carries the
    /// confirmed reading untouched *and* a projection that has moved past it.
    #[test]
    fn a_source_that_stopped_publishing_still_gets_a_moving_number() {
        use crate::domain::{
            CostInfo, DisplayTotal, SourceProvenance, TimestampInterpretation, TokenCounts,
            TokenField, TotalRule,
        };

        let repository = InMemoryUsageRepository::new();
        let now = Utc::now();
        let ago = |minutes: i64| now - chrono::Duration::minutes(minutes);
        let resets_at = Some(now + chrono::Duration::hours(3));

        let burn = |minutes: i64, index: u64| UsageRecord {
            normalization_version: 1,
            id: format!("burn-{index}"),
            raw_timestamp: None,
            event_timestamp_utc: Some(ago(minutes)),
            timestamp_interpretation: TimestampInterpretation::ExplicitOffset,
            source_app: SourceApp::ClaudeCode,
            source_event_id: Some(format!("burn-{index}")),
            dedupe_key: format!("burn-{index}"),
            dedupe_algorithm_version: 1,
            content_hash: format!("hash-{index}"),
            content_hash_version: 1,
            provider: None,
            model: Some("claude-opus-5".to_string()),
            tokens: TokenCounts {
                input: TokenField::exact(100_000),
                output: TokenField::exact(10_000),
                cached_input: TokenField::exact(0),
                reasoning: TokenField::unknown(),
            },
            reported_total_tokens: None,
            display_total: Some(DisplayTotal {
                tokens: 110_000,
                rule: TotalRule::InputPlusOutput,
            }),
            project: None,
            session_id: None,
            cost: CostInfo::not_available(),
            imported_at: ago(minutes),
            provenance: SourceProvenance {
                adapter_id: "claude_code".to_string(),
                adapter_version: "0.3.0".to_string(),
                source_ref: None,
            },
        };

        // Four confirmed readings, ten minutes apart, each preceded by one
        // call — the history a fitted rate is built from. The last lands 10
        // minutes ago, which is where the source went quiet.
        for (step, used) in [(50i64, 0u16), (40, 100), (30, 200), (20, 300)] {
            let transaction = SourceTransaction {
                records: vec![burn(step + 5, step as u64)],
                quotas: vec![UsageQuota {
                    source_app: SourceApp::ClaudeCode,
                    label: None,
                    window_minutes: 300,
                    used_percent_tenths: used,
                    resets_at,
                    observed_at: ago(step),
                }],
                ..SourceTransaction::new(SourceApp::ClaudeCode, "claude_code", ago(step))
            };
            repository.commit(transaction).unwrap();
        }

        // Work continues after the last reading; the source says nothing.
        repository
            .commit(SourceTransaction {
                records: vec![burn(5, 999)],
                ..SourceTransaction::new(SourceApp::ClaudeCode, "claude_code", now)
            })
            .unwrap();

        let snapshot = repository
            .snapshot(&SummaryQuery::default(), &RecentQuery::default())
            .unwrap();

        // The reported figure is left exactly as reported.
        let quota = snapshot
            .quotas
            .iter()
            .find(|quota| quota.window_minutes == 300)
            .expect("the session window is stored");
        assert_eq!(quota.used_percent_tenths, 300);

        // And beside it, the same window carried forward by the spend since.
        let projection = snapshot
            .projections
            .iter()
            .find(|projection| projection.window_minutes == 300)
            .expect("a stale reading with fresh records earns a projection");
        assert_eq!(projection.confirmed_percent_tenths, 300);
        assert_eq!(projection.added_percent_tenths, 100);
        assert_eq!(projection.projected_percent_tenths, 400);
        assert_eq!(projection.pairs, 3);
        assert_eq!(projection.residual_percent, 0);
    }

    struct RevisionFirstReader {
        inner: InMemoryUsageRepository,
        revision_read: AtomicBool,
    }

    impl RevisionFirstReader {
        fn assert_revision_was_read(&self) {
            assert!(
                self.revision_read.load(Ordering::SeqCst),
                "snapshot payload was read before its revision"
            );
        }
    }

    impl UsageReader for RevisionFirstReader {
        fn summary(&self, query: &SummaryQuery) -> Result<UsageSummary, RepositoryError> {
            self.assert_revision_was_read();
            self.inner.summary(query)
        }

        fn recent(&self, query: &RecentQuery) -> Result<Vec<UsageRecord>, RepositoryError> {
            self.assert_revision_was_read();
            self.inner.recent(query)
        }

        fn thread_usage(
            &self,
            query: &ThreadUsageQuery,
        ) -> Result<ThreadUsageReport, RepositoryError> {
            self.assert_revision_was_read();
            self.inner.thread_usage(query)
        }

        fn records_since(&self, since: DateTime<Utc>) -> Result<Vec<UsageRecord>, RepositoryError> {
            self.assert_revision_was_read();
            self.inner.records_since(since)
        }

        fn count(&self) -> Result<usize, RepositoryError> {
            self.assert_revision_was_read();
            self.inner.count()
        }

        fn quotas(&self) -> Result<Vec<UsageQuota>, RepositoryError> {
            self.assert_revision_was_read();
            self.inner.quotas()
        }

        fn health(&self) -> Result<Vec<SourceSyncHealth>, RepositoryError> {
            self.assert_revision_was_read();
            self.inner.health()
        }

        fn revision(&self) -> Result<RepositoryRevision, RepositoryError> {
            self.revision_read.store(true, Ordering::SeqCst);
            self.inner.revision()
        }

        fn checkpoints(&self, adapter_id: &str) -> Result<Vec<SourceCheckpoint>, RepositoryError> {
            self.assert_revision_was_read();
            self.inner.checkpoints(adapter_id)
        }

        fn quota_samples(&self, since: DateTime<Utc>) -> Result<Vec<QuotaSample>, RepositoryError> {
            self.assert_revision_was_read();
            self.inner.quota_samples(since)
        }

        fn snapshot(
            &self,
            overview: &SummaryQuery,
            recent: &RecentQuery,
        ) -> Result<DashboardSnapshot, RepositoryError> {
            assemble_snapshot(self, overview, recent)
        }

        fn risk_snapshot(&self) -> Result<RiskSnapshot, RepositoryError> {
            assemble_risk_snapshot(self)
        }

        fn pace(&self) -> Result<Vec<WindowPace>, RepositoryError> {
            assemble_pace_only(self)
        }
    }

    #[test]
    fn snapshot_reads_revision_before_any_payload() {
        let reader = RevisionFirstReader {
            inner: InMemoryUsageRepository::new(),
            revision_read: AtomicBool::new(false),
        };

        reader
            .snapshot(&SummaryQuery::default(), &RecentQuery::default())
            .unwrap();
    }

    #[test]
    fn the_risk_snapshot_reads_revision_before_any_payload_too() {
        let reader = RevisionFirstReader {
            inner: InMemoryUsageRepository::new(),
            revision_read: AtomicBool::new(false),
        };

        reader.risk_snapshot().unwrap();
    }

    /// The narrow read is an optimisation, and an optimisation that answers a
    /// different question is a bug. Whatever the two reads have in common they
    /// must agree on exactly, or the compact window and the dashboard would
    /// show different allowances for the same revision.
    #[test]
    fn the_two_reads_agree_on_everything_they_share() {
        let repository = InMemoryUsageRepository::new();
        let now = Utc.timestamp_opt(1_785_200_000, 0).unwrap();
        let mut transaction = SourceTransaction::new(SourceApp::Codex, "codex", now);
        transaction.quotas = vec![UsageQuota {
            source_app: SourceApp::Codex,
            label: None,
            window_minutes: 10_080,
            used_percent_tenths: 690,
            resets_at: Some(now + chrono::Duration::days(3)),
            observed_at: now,
        }];
        transaction.health = HealthOutcome::Succeeded {
            source_observed_at: Some(now),
            awaiting_upstream: false,
        };
        repository.commit(transaction).unwrap();

        let full = repository
            .snapshot(&SummaryQuery::default(), &RecentQuery::default())
            .unwrap();
        let risk = repository.risk_snapshot().unwrap();

        assert_eq!(full.revision, risk.revision);
        assert_eq!(full.quotas, risk.quotas);
        assert_eq!(full.health, risk.health);
        assert_eq!(full.pace, risk.pace);
        assert_eq!(full.projections, risk.projections);
    }
}
