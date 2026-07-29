//! In-memory usage store.
//!
//! The MVP's only backend. It holds normalized records for the life of the
//! process and is replaced by SQLite once real log importing begins.

use std::collections::HashSet;
use std::sync::RwLock;

use chrono::Utc;

use crate::domain::{RecentQuery, SummaryQuery, UsageRecord, UsageSummary};

use super::summarize;
use super::{InsertReport, RepositoryError, UsageRepository};

#[derive(Debug, Default)]
struct Store {
    records: Vec<UsageRecord>,
    dedupe_keys: HashSet<String>,
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
        // Insertion cannot fail on a freshly created lock.
        let _ = repository.insert_batch(records);
        repository
    }
}

impl UsageRepository for InMemoryUsageRepository {
    fn insert_batch(&self, records: Vec<UsageRecord>) -> Result<InsertReport, RepositoryError> {
        let mut store = self.store.write().map_err(|_| RepositoryError::Unavailable)?;
        let mut report = InsertReport::default();

        for record in records {
            if store.dedupe_keys.insert(record.dedupe_key.clone()) {
                store.records.push(record);
                report.inserted += 1;
            } else {
                report.duplicates_skipped += 1;
            }
        }

        Ok(report)
    }

    fn summary(&self, query: &SummaryQuery) -> Result<UsageSummary, RepositoryError> {
        let store = self.store.read().map_err(|_| RepositoryError::Unavailable)?;
        let undated = summarize::undated_excluded(store.records.iter(), query);
        let matching = store.records.iter().filter(|record| summarize::matches(record, query));
        Ok(summarize::summarize(matching, Utc::now(), undated))
    }

    fn recent(&self, query: &RecentQuery) -> Result<Vec<UsageRecord>, RepositoryError> {
        let store = self.store.read().map_err(|_| RepositoryError::Unavailable)?;
        let matching = store
            .records
            .iter()
            .filter(|record| summarize::matches(record, &query.filter))
            .collect();
        Ok(summarize::take_recent(matching, query))
    }

    fn count(&self) -> Result<usize, RepositoryError> {
        let store = self.store.read().map_err(|_| RepositoryError::Unavailable)?;
        Ok(store.records.len())
    }

    fn clear(&self) -> Result<(), RepositoryError> {
        let mut store = self.store.write().map_err(|_| RepositoryError::Unavailable)?;
        store.records.clear();
        store.dedupe_keys.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CostCalculationStatus, CostDraft, Money, SourceApp, SourceProvenance, TokenCounts,
        TokenField, UsageRecordDraft,
    };
    use crate::normalize;
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
        normalize::normalize_at(draft, Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap()).unwrap()
    }

    fn instant(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, day, 0, 0, 0).unwrap()
    }

    #[test]
    fn skips_records_it_already_holds() {
        let repository = InMemoryUsageRepository::new();
        let first = repository
            .insert_batch(vec![record(SourceApp::Cursor, 1, Some("m"), Some(5))])
            .unwrap();
        let second = repository
            .insert_batch(vec![record(SourceApp::Cursor, 1, Some("m"), Some(5))])
            .unwrap();

        assert_eq!(first, InsertReport { inserted: 1, duplicates_skipped: 0 });
        assert_eq!(second, InsertReport { inserted: 0, duplicates_skipped: 1 });
        assert_eq!(repository.count().unwrap(), 1);
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

        let unknown_model = summary.by_model.iter().find(|group| group.key.is_none()).unwrap();
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
            .summary(&SummaryQuery { from: Some(instant(1)), ..SummaryQuery::default() })
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

        let recent = repository.recent(&RecentQuery { limit: 2, ..RecentQuery::default() }).unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].raw_timestamp.as_deref(), Some("2026-07-20T10:00:00Z"));
        assert_eq!(recent[1].raw_timestamp.as_deref(), Some("2026-07-10T10:00:00Z"));
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

    #[test]
    fn clear_empties_the_store_and_its_dedupe_keys() {
        let repository = InMemoryUsageRepository::with_records(vec![record(
            SourceApp::Cursor,
            1,
            Some("opus"),
            Some(1),
        )]);
        repository.clear().unwrap();
        assert_eq!(repository.count().unwrap(), 0);

        let reinserted = repository
            .insert_batch(vec![record(SourceApp::Cursor, 1, Some("opus"), Some(1))])
            .unwrap();
        assert_eq!(reinserted.inserted, 1);
    }
}
