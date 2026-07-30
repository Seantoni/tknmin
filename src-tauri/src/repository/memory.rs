//! In-memory usage store.
//!
//! Kept for focused tests, where a temporary file and a migration are noise.
//! It implements exactly the same transaction semantics as the SQLite backend
//! — upsert by identity, atomic commit, monotonic revision — so a test that
//! passes here means something about production.

use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

use chrono::{DateTime, Utc};

use crate::domain::{
    same_window_instance, QuotaSample, RecentQuery, RepositoryRevision, SourceApp,
    SourceCheckpoint, SourceSyncHealth, SummaryQuery, UsageQuota, UsageRecord, UsageSummary,
    WindowPace,
};

use super::summarize;
use super::{
    apply_health, assemble_pace_only, assemble_snapshot, materially_differs, merge_quota,
    quota_key, CommitCounts, CommitOutcome, DashboardSnapshot, RepositoryError, SourceTransaction,
    UsageReader, UsageWriter,
};

#[derive(Debug, Default)]
struct Store {
    /// Keyed by identity, so a corrected snapshot of an event lands on the row
    /// the first one created rather than beside it.
    records: HashMap<String, UsageRecord>,
    quotas: Vec<UsageQuota>,
    quota_samples: Vec<QuotaSample>,
    health: Vec<SourceSyncHealth>,
    checkpoints: HashMap<(String, String), SourceCheckpoint>,
    revision: RepositoryRevision,
}

#[derive(Debug, Default)]
pub struct InMemoryUsageRepository {
    store: RwLock<Store>,
}

impl InMemoryUsageRepository {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a repository already holding records, for fixtures and tests.
    pub fn with_records(records: Vec<UsageRecord>) -> Self {
        let repository = Self::new();
        // Committing cannot fail on a freshly created lock.
        let _ = repository.commit(SourceTransaction {
            records,
            ..SourceTransaction::new(SourceApp::Cursor, "fixture", Utc::now())
        });
        repository
    }

    fn read(&self) -> Result<std::sync::RwLockReadGuard<'_, Store>, RepositoryError> {
        self.store.read().map_err(|_| RepositoryError::Unavailable)
    }
}

impl UsageReader for InMemoryUsageRepository {
    fn summary(&self, query: &SummaryQuery) -> Result<UsageSummary, RepositoryError> {
        let store = self.read()?;
        let undated = summarize::undated_excluded(store.records.values(), query);
        let matching = store
            .records
            .values()
            .filter(|record| summarize::matches(record, query));
        Ok(summarize::summarize(matching, Utc::now(), undated))
    }

    fn recent(&self, query: &RecentQuery) -> Result<Vec<UsageRecord>, RepositoryError> {
        let store = self.read()?;
        let matching = store
            .records
            .values()
            .filter(|record| summarize::matches(record, &query.filter))
            .collect();
        Ok(summarize::take_recent(matching, query))
    }

    fn records_since(&self, since: DateTime<Utc>) -> Result<Vec<UsageRecord>, RepositoryError> {
        Ok(self
            .read()?
            .records
            .values()
            .filter(|record| {
                record
                    .event_timestamp_utc
                    .is_some_and(|event| event >= since)
            })
            .cloned()
            .collect())
    }

    fn count(&self) -> Result<usize, RepositoryError> {
        Ok(self.read()?.records.len())
    }

    fn quotas(&self) -> Result<Vec<UsageQuota>, RepositoryError> {
        Ok(self.read()?.quotas.clone())
    }

    fn quota_samples(&self, since: DateTime<Utc>) -> Result<Vec<QuotaSample>, RepositoryError> {
        let mut samples: Vec<QuotaSample> = self
            .read()?
            .quota_samples
            .iter()
            .filter(|sample| sample.observed_at >= since)
            .cloned()
            .collect();
        samples.sort_by_key(|sample| sample.observed_at);
        Ok(samples)
    }

    fn health(&self) -> Result<Vec<SourceSyncHealth>, RepositoryError> {
        Ok(self.read()?.health.clone())
    }

    fn revision(&self) -> Result<RepositoryRevision, RepositoryError> {
        Ok(self.read()?.revision)
    }

    fn checkpoints(&self, adapter_id: &str) -> Result<Vec<SourceCheckpoint>, RepositoryError> {
        Ok(self
            .read()?
            .checkpoints
            .values()
            .filter(|checkpoint| checkpoint.adapter_id == adapter_id)
            .cloned()
            .collect())
    }

    fn snapshot(
        &self,
        overview: &SummaryQuery,
        recent: &RecentQuery,
    ) -> Result<DashboardSnapshot, RepositoryError> {
        assemble_snapshot(self, overview, recent)
    }

    fn pace(&self) -> Result<Vec<WindowPace>, RepositoryError> {
        assemble_pace_only(self)
    }
}

impl UsageWriter for InMemoryUsageRepository {
    fn commit(&self, transaction: SourceTransaction) -> Result<CommitOutcome, RepositoryError> {
        let mut store = self
            .store
            .write()
            .map_err(|_| RepositoryError::Unavailable)?;
        let mut counts = CommitCounts::default();

        // Replacement first, and never touching the identities this batch is
        // about to write: a scope exists to drop what the source stopped
        // reporting, not to churn what it still does.
        let keep: HashSet<&str> = transaction
            .records
            .iter()
            .map(|record| record.dedupe_key.as_str())
            .collect();
        for scope in &transaction.replace_scopes {
            let doomed: Vec<String> = store
                .records
                .values()
                .filter(|record| {
                    record.source_app == scope.source_app
                        && record.provenance.adapter_id == scope.adapter_id
                        && record
                            .event_timestamp_utc
                            .is_some_and(|at| at >= scope.from && at < scope.until)
                        && !keep.contains(record.dedupe_key.as_str())
                })
                .map(|record| record.dedupe_key.clone())
                .collect();
            counts.deleted += doomed.len();
            for key in doomed {
                store.records.remove(&key);
            }
        }

        for record in transaction.records {
            match store.records.get(&record.dedupe_key) {
                Some(stored) if stored.content_hash == record.content_hash => {
                    counts.unchanged += 1;
                }
                Some(_) => {
                    counts.updated += 1;
                    store.records.insert(record.dedupe_key.clone(), record);
                }
                None => {
                    counts.inserted += 1;
                    store.records.insert(record.dedupe_key.clone(), record);
                }
            }
        }

        let mut quotas_changed = false;
        if transaction.replace_quotas {
            let reported: Vec<_> = transaction.quotas.iter().map(quota_key).collect();
            let before = store.quotas.len();
            store.quotas.retain(|stored| {
                stored.source_app != transaction.source_app || reported.contains(&quota_key(stored))
            });
            quotas_changed |= store.quotas.len() != before;
        }
        for quota in transaction.quotas {
            let incoming = quota.clone();
            if merge_quota(&mut store.quotas, quota) {
                quotas_changed = true;
                // Sample for pace history, on the same change-only rule the
                // SQLite backend uses, so the two stay checkable against each
                // other.
                let moved = store
                    .quota_samples
                    .iter()
                    .rev()
                    .find(|sample| {
                        sample.source_app == incoming.source_app
                            && sample.label == incoming.label
                            && sample.window_minutes == incoming.window_minutes
                    })
                    .is_none_or(|sample| {
                        sample.used_percent_tenths != incoming.used_percent_tenths
                            || !same_window_instance(sample.resets_at, incoming.resets_at)
                    });
                if moved {
                    store.quota_samples.push(QuotaSample {
                        source_app: incoming.source_app,
                        label: incoming.label,
                        window_minutes: incoming.window_minutes,
                        used_percent_tenths: incoming.used_percent_tenths,
                        resets_at: incoming.resets_at,
                        observed_at: incoming.observed_at,
                    });
                }
            }
        }

        for checkpoint in transaction.checkpoints {
            store.checkpoints.insert(
                (checkpoint.adapter_id.clone(), checkpoint.source_key.clone()),
                checkpoint,
            );
        }

        let source_app = transaction.source_app;
        if !store
            .health
            .iter()
            .any(|health| health.source_app == source_app)
        {
            store.health.push(SourceSyncHealth::unknown(source_app));
        }
        let health = store
            .health
            .iter_mut()
            .find(|health| health.source_app == source_app)
            .expect("inserted above when missing");
        let before = health.clone();
        apply_health(health, &transaction.health, transaction.committed_at);
        let health_changed = materially_differs(health, &before);

        let data_changed = counts.changed_records() || quotas_changed;
        let advanced = data_changed || health_changed;
        if advanced {
            store.revision += 1;
        }

        Ok(CommitOutcome {
            revision: store.revision,
            counts,
            advanced,
            data_changed,
        })
    }

    fn prune_quota_samples(&self, before: DateTime<Utc>) -> Result<usize, RepositoryError> {
        let mut store = self
            .store
            .write()
            .map_err(|_| RepositoryError::Unavailable)?;
        let held = store.quota_samples.len();
        store
            .quota_samples
            .retain(|sample| sample.observed_at >= before);
        Ok(held - store.quota_samples.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CostCalculationStatus, CostDraft, Money, ReplaceScope, SourceProvenance, SyncState,
        TokenCounts, TokenField, UsageRecordDraft,
    };
    use crate::normalize;
    use crate::repository::HealthOutcome;
    use chrono::{DateTime, TimeZone};

    fn provenance() -> SourceProvenance {
        SourceProvenance {
            adapter_id: "test".to_string(),
            adapter_version: "0.1.0".to_string(),
            source_ref: None,
        }
    }

    fn record(source: SourceApp, day: u32, model: Option<&str>, input: Option<u64>) -> UsageRecord {
        let mut draft = UsageRecordDraft::new(source, provenance())
            .with_raw_timestamp(format!("2026-07-{day:02}T10:00:00Z"))
            .with_tokens(TokenCounts {
                input: input.map(TokenField::exact).unwrap_or_default(),
                output: TokenField::exact(10),
                ..TokenCounts::default()
            });
        draft.model = model.map(str::to_string);
        normalize::normalize_at(draft, Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap())
            .unwrap()
    }

    fn instant(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, day, 0, 0, 0).unwrap()
    }

    fn usage(records: Vec<UsageRecord>) -> SourceTransaction {
        SourceTransaction {
            records,
            health: HealthOutcome::Succeeded {
                source_observed_at: None,
                awaiting_upstream: false,
            },
            ..SourceTransaction::new(SourceApp::Cursor, "test", Utc::now())
        }
    }

    #[test]
    fn replaying_identical_content_is_a_no_op() {
        let repository = InMemoryUsageRepository::new();
        let first = repository
            .commit(usage(vec![record(
                SourceApp::Cursor,
                1,
                Some("m"),
                Some(5),
            )]))
            .unwrap();
        let second = repository
            .commit(usage(vec![record(
                SourceApp::Cursor,
                1,
                Some("m"),
                Some(5),
            )]))
            .unwrap();

        assert_eq!(first.counts.inserted, 1);
        assert_eq!(second.counts.unchanged, 1);
        assert_eq!(second.counts.inserted, 0);
        assert_eq!(repository.count().unwrap(), 1);
    }

    #[test]
    fn a_later_snapshot_of_the_same_event_replaces_the_earlier_one() {
        let interim = |output: u64| {
            let draft = UsageRecordDraft::new(SourceApp::ClaudeCode, provenance())
                .with_source_event_id("msg_01")
                .with_raw_timestamp("2026-07-29T10:00:00Z")
                .with_tokens(TokenCounts {
                    input: TokenField::exact(100),
                    output: TokenField::exact(output),
                    ..TokenCounts::default()
                });
            normalize::normalize_at(draft, instant(29)).unwrap()
        };

        let repository = InMemoryUsageRepository::new();
        repository.commit(usage(vec![interim(5)])).unwrap();
        let settled = repository.commit(usage(vec![interim(120)])).unwrap();

        assert_eq!(settled.counts.updated, 1);
        assert_eq!(repository.count().unwrap(), 1);
        let summary = repository.summary(&SummaryQuery::default()).unwrap();
        assert_eq!(summary.totals.output.tokens, 120);
    }

    #[test]
    fn a_replace_scope_drops_events_the_source_no_longer_reports() {
        let repository = InMemoryUsageRepository::new();
        repository
            .commit(usage(vec![
                record(SourceApp::Cursor, 10, Some("m"), Some(1)),
                record(SourceApp::Cursor, 11, Some("m"), Some(2)),
            ]))
            .unwrap();

        let outcome = repository
            .commit(SourceTransaction {
                replace_scopes: vec![ReplaceScope {
                    source_app: SourceApp::Cursor,
                    adapter_id: "test".to_string(),
                    from: instant(10),
                    until: instant(12),
                }],
                ..usage(vec![record(SourceApp::Cursor, 10, Some("m"), Some(1))])
            })
            .unwrap();

        assert_eq!(outcome.counts.deleted, 1);
        assert_eq!(repository.count().unwrap(), 1);
    }

    #[test]
    fn a_failed_attempt_preserves_the_data_it_had() {
        let repository = InMemoryUsageRepository::new();
        repository
            .commit(usage(vec![record(
                SourceApp::Cursor,
                1,
                Some("m"),
                Some(5),
            )]))
            .unwrap();

        let outcome = repository
            .commit(SourceTransaction::health_only(
                SourceApp::Cursor,
                "test",
                HealthOutcome::Failed {
                    offline: true,
                    error: "no network".to_string(),
                },
                Utc::now(),
            ))
            .unwrap();

        assert_eq!(repository.count().unwrap(), 1);
        assert!(!outcome.counts.changed_records());
        assert_eq!(repository.health().unwrap()[0].state, SyncState::Offline);
    }

    #[test]
    fn a_transaction_that_moves_nothing_does_not_advance_the_revision() {
        let repository = InMemoryUsageRepository::new();
        let at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        let repeat = || SourceTransaction {
            committed_at: at,
            ..usage(vec![record(SourceApp::Cursor, 1, Some("m"), Some(5))])
        };

        repository.commit(repeat()).unwrap();
        let settled = repository.revision().unwrap();
        let second = repository.commit(repeat()).unwrap();

        assert!(!second.advanced);
        assert_eq!(second.revision, settled);
    }

    #[test]
    fn checkpoints_round_trip_per_adapter() {
        let repository = InMemoryUsageRepository::new();
        repository
            .commit(SourceTransaction {
                checkpoints: vec![SourceCheckpoint {
                    adapter_id: "codex".to_string(),
                    source_key: "rollout-1".to_string(),
                    payload: serde_json::json!({ "offset": 4096 }),
                }],
                ..SourceTransaction::new(SourceApp::Codex, "codex", Utc::now())
            })
            .unwrap();

        let stored = repository.checkpoints("codex").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].payload["offset"], 4096);
        assert!(repository.checkpoints("cursor").unwrap().is_empty());
    }

    #[test]
    fn one_failing_source_does_not_remove_another_sources_quota() {
        let repository = InMemoryUsageRepository::new();
        let quota = |source: SourceApp| UsageQuota {
            source_app: source,
            label: None,
            window_minutes: 10_080,
            used_percent_tenths: 300,
            resets_at: None,
            observed_at: Utc::now(),
        };
        for source in [SourceApp::Codex, SourceApp::ClaudeCode] {
            repository
                .commit(SourceTransaction {
                    quotas: vec![quota(source)],
                    replace_quotas: true,
                    ..SourceTransaction::new(source, "test", Utc::now())
                })
                .unwrap();
        }

        repository
            .commit(SourceTransaction::health_only(
                SourceApp::Codex,
                "test",
                HealthOutcome::Failed {
                    offline: false,
                    error: "unreadable".to_string(),
                },
                Utc::now(),
            ))
            .unwrap();

        assert_eq!(repository.quotas().unwrap().len(), 2);
    }

    #[test]
    fn totals_count_known_values_and_report_unknown_ones() {
        let repository = InMemoryUsageRepository::with_records(vec![
            record(SourceApp::Cursor, 1, Some("opus"), Some(100)),
            record(SourceApp::Codex, 2, Some("opus"), None),
        ]);

        let summary = repository.summary(&SummaryQuery::default()).unwrap();
        assert_eq!(summary.totals.record_count, 2);
        assert_eq!(summary.totals.input.tokens, 100);
        assert_eq!(summary.totals.input.counted_records, 1);
        assert_eq!(summary.totals.input.unknown_records, 1);
        assert_eq!(summary.totals.output.tokens, 20);
    }

    #[test]
    fn breaks_down_by_source_and_model() {
        let repository = InMemoryUsageRepository::with_records(vec![
            record(SourceApp::Cursor, 1, Some("opus"), Some(100)),
            record(SourceApp::Cursor, 2, None, Some(50)),
            record(SourceApp::Codex, 3, Some("opus"), Some(10)),
        ]);

        let summary = repository.summary(&SummaryQuery::default()).unwrap();
        assert_eq!(summary.by_source.len(), 2);
        assert_eq!(summary.by_source[0].key.as_deref(), Some("cursor"));
        assert_eq!(summary.by_source[0].totals.record_count, 2);

        let unknown_model = summary
            .by_model
            .iter()
            .find(|group| group.key.is_none())
            .unwrap();
        assert_eq!(unknown_model.label, "Unknown model");
        assert_eq!(unknown_model.totals.record_count, 1);
    }

    #[test]
    fn filters_by_source_and_date_range() {
        let repository = InMemoryUsageRepository::with_records(vec![
            record(SourceApp::Cursor, 1, Some("opus"), Some(100)),
            record(SourceApp::Cursor, 10, Some("opus"), Some(100)),
            record(SourceApp::Codex, 10, Some("opus"), Some(100)),
        ]);

        let query = SummaryQuery {
            sources: Some(vec![SourceApp::Cursor]),
            from: Some(instant(5)),
            ..SummaryQuery::default()
        };
        let summary = repository.summary(&query).unwrap();
        assert_eq!(summary.totals.record_count, 1);
    }

    #[test]
    fn reports_undated_records_excluded_by_a_date_filter() {
        let mut undated = UsageRecordDraft::new(SourceApp::Codex, provenance());
        undated.tokens.input = TokenField::exact(7);
        let undated = normalize::normalize_at(undated, instant(29)).unwrap();

        let repository = InMemoryUsageRepository::with_records(vec![
            record(SourceApp::Cursor, 10, Some("opus"), Some(100)),
            undated,
        ]);

        let unfiltered = repository.summary(&SummaryQuery::default()).unwrap();
        assert_eq!(unfiltered.totals.record_count, 2);
        assert_eq!(unfiltered.undated_records_excluded, 0);

        let filtered = repository
            .summary(&SummaryQuery {
                from: Some(instant(1)),
                ..SummaryQuery::default()
            })
            .unwrap();
        assert_eq!(filtered.totals.record_count, 1);
        assert_eq!(filtered.undated_records_excluded, 1);
    }

    #[test]
    fn recent_returns_newest_first_within_the_limit() {
        let repository = InMemoryUsageRepository::with_records(vec![
            record(SourceApp::Cursor, 1, Some("opus"), Some(1)),
            record(SourceApp::Cursor, 20, Some("opus"), Some(2)),
            record(SourceApp::Cursor, 10, Some("opus"), Some(3)),
        ]);

        let recent = repository
            .recent(&RecentQuery {
                limit: 2,
                ..RecentQuery::default()
            })
            .unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(
            recent[0].raw_timestamp.as_deref(),
            Some("2026-07-20T10:00:00Z")
        );
        assert_eq!(
            recent[1].raw_timestamp.as_deref(),
            Some("2026-07-10T10:00:00Z")
        );
    }

    #[test]
    fn keeps_currencies_separate_in_cost_totals() {
        let with_cost = |currency: &str, minor: i64, day: u32| {
            let draft = UsageRecordDraft::new(SourceApp::Cursor, provenance())
                .with_raw_timestamp(format!("2026-07-{day:02}T10:00:00Z"))
                .with_cost(CostDraft {
                    amount: Some(Money::new(minor, currency, 2).unwrap()),
                    status: CostCalculationStatus::ReportedBySource,
                    pricing_version: None,
                });
            normalize::normalize_at(draft, instant(29)).unwrap()
        };

        let repository = InMemoryUsageRepository::with_records(vec![
            with_cost("USD", 125, 1),
            with_cost("USD", 75, 2),
            with_cost("EUR", 100, 3),
            record(SourceApp::Cursor, 4, Some("opus"), Some(1)),
        ]);

        let summary = repository.summary(&SummaryQuery::default()).unwrap();
        assert_eq!(summary.totals.cost.by_currency.len(), 2);
        assert_eq!(summary.totals.cost.records_without_cost, 1);

        let usd = summary
            .totals
            .cost
            .by_currency
            .iter()
            .find(|entry| entry.amount.currency == "USD")
            .unwrap();
        assert_eq!(usd.amount, Money::new(200, "USD", 2).unwrap());
        assert_eq!(usd.counted_records, 2);
    }

    /// A record of `tokens` input at an exact instant, for history tests.
    fn burned(source: SourceApp, at: DateTime<Utc>, tokens: u64) -> UsageRecord {
        let draft = UsageRecordDraft::new(source, provenance())
            .with_raw_timestamp(at.to_rfc3339())
            .with_tokens(TokenCounts {
                input: TokenField::exact(tokens),
                output: TokenField::exact(0),
                ..TokenCounts::default()
            });
        normalize::normalize_at(draft, at).unwrap()
    }

    #[test]
    fn risk_reads_history_the_dashboard_filter_cannot_hide() {
        // The baseline and the current burn must not depend on what the
        // dashboard happens to be showing: a filter is a way of looking at the
        // data, not a change to how much allowance is left.
        let now = Utc::now();
        // The rate is measured over the trailing hour, so it is judged against
        // the hour that window mostly covers — the one 30 minutes back.
        let burn_at = now - chrono::Duration::minutes(30);

        let mut records = Vec::new();
        for day_back in 1..=5 {
            records.push(burned(
                SourceApp::Codex,
                burn_at - chrono::Duration::days(day_back),
                10_000,
            ));
        }
        // Today's burn, at exactly the usual rate for this hour.
        records.push(burned(SourceApp::Codex, burn_at, 10_000));
        // Sixty newer records from another source — more than the recent
        // list's default limit, so a baseline drawn from that list would see
        // none of the history above.
        for n in 0..60u64 {
            records.push(burned(
                SourceApp::Cursor,
                now - chrono::Duration::seconds(n as i64),
                n + 1,
            ));
        }

        let repository = InMemoryUsageRepository::with_records(records);
        repository
            .commit(SourceTransaction {
                quotas: vec![UsageQuota {
                    source_app: SourceApp::Codex,
                    label: None,
                    window_minutes: 300,
                    used_percent_tenths: 500,
                    resets_at: Some(now + chrono::Duration::hours(2)),
                    observed_at: now,
                }],
                ..SourceTransaction::new(SourceApp::Codex, "test", now)
            })
            .unwrap();

        let codex_verdict = |recent: &RecentQuery| {
            repository
                .snapshot(&SummaryQuery::default(), recent)
                .unwrap()
                .pace
                .into_iter()
                .find(|pace| pace.source_app == SourceApp::Codex)
                .expect("the live Codex window has a pace row")
                .vs_baseline
        };

        assert_eq!(
            codex_verdict(&RecentQuery::default()),
            crate::domain::PaceVsBaseline::Typical,
            "a burn at the usual rate for this hour read as something else"
        );
        // Looking at another source, one row at a time, changes nothing.
        assert_eq!(
            codex_verdict(&RecentQuery {
                limit: 1,
                filter: SummaryQuery {
                    sources: Some(vec![SourceApp::ClaudeCode]),
                    ..SummaryQuery::default()
                },
            }),
            crate::domain::PaceVsBaseline::Typical,
        );
    }

    #[test]
    fn records_since_ignores_limits_and_filters() {
        let now = Utc::now();
        let mut records = vec![burned(
            SourceApp::Codex,
            now - chrono::Duration::days(45),
            1_000,
        )];
        for n in 0..100u64 {
            records.push(burned(
                SourceApp::Cursor,
                now - chrono::Duration::minutes(n as i64),
                n + 1,
            ));
        }
        let repository = InMemoryUsageRepository::with_records(records);

        let since = repository
            .records_since(now - chrono::Duration::days(30))
            .unwrap();
        assert_eq!(since.len(), 100, "the cutoff is the only thing that bounds");
    }

    #[test]
    fn a_snapshot_reads_every_field_at_one_revision() {
        let repository = InMemoryUsageRepository::with_records(vec![record(
            SourceApp::Cursor,
            1,
            Some("opus"),
            Some(1),
        )]);
        let snapshot = repository
            .snapshot(&SummaryQuery::default(), &RecentQuery::default())
            .unwrap();

        assert_eq!(snapshot.revision, repository.revision().unwrap());
        assert_eq!(snapshot.record_count, 1);
        assert_eq!(snapshot.overview.totals.record_count, 1);
    }
}
