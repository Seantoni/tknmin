//! Refreshing the store from real logs.
//!
//! One pass over every registered adapter: discover, read, parse, import.
//! Each stage fails independently — a source that is not implemented yet, a
//! file that vanished mid-scan, a malformed entry — without affecting the
//! rest, and the report says exactly what happened per source.

use serde::{Deserialize, Serialize};

use crate::adapters::{self, AdapterError, SourceAdapter};
use crate::domain::UsageQuota;
use crate::pipeline::{self};
use crate::repository::UsageRepository;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshStatus {
    /// The adapter read its logs and imported what it found.
    Imported,
    /// The adapter is still a contract-only stub.
    Planned,
    /// Discovery itself failed; the `failures` list says why.
    Failed,
}

/// What one adapter did during a refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceRefreshReport {
    pub adapter: String,
    pub status: RefreshStatus,
    pub sources_found: usize,
    pub sources_failed: usize,
    pub drafts_parsed: usize,
    pub inserted: usize,
    pub duplicates_skipped: usize,
    pub rejected: usize,
    /// Human-readable failures, capped so a broken tree cannot flood the UI.
    pub failures: Vec<String>,
}

impl SourceRefreshReport {
    fn new(adapter: &str) -> Self {
        Self {
            adapter: adapter.to_string(),
            status: RefreshStatus::Imported,
            sources_found: 0,
            sources_failed: 0,
            drafts_parsed: 0,
            inserted: 0,
            duplicates_skipped: 0,
            rejected: 0,
            failures: Vec::new(),
        }
    }

    fn note_failure(&mut self, failure: String) {
        self.sources_failed += 1;
        const MAX_REPORTED_FAILURES: usize = 10;
        if self.failures.len() < MAX_REPORTED_FAILURES {
            self.failures.push(failure);
        }
    }
}

/// The outcome of one refresh across every source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshReport {
    pub sources: Vec<SourceRefreshReport>,
    /// The freshest quota snapshot each source reported, if it reports one.
    pub quotas: Vec<UsageQuota>,
}

impl RefreshReport {
    pub fn total_inserted(&self) -> usize {
        self.sources.iter().map(|source| source.inserted).sum()
    }
}

/// Rescan every registered adapter's logs and import whatever is new.
pub fn refresh_all(repository: &dyn UsageRepository) -> RefreshReport {
    refresh_with(&adapters::registry(), repository)
}

/// Quota snapshots only — no log walk. Used by the background alert poll.
pub fn refresh_quotas() -> Vec<UsageQuota> {
    adapters::registry()
        .iter()
        .filter_map(|adapter| adapter.quotas().ok())
        .flatten()
        .collect()
}

/// The same pass over an explicit adapter set, so tests never touch real logs.
pub fn refresh_with(
    adapters: &[Box<dyn SourceAdapter>],
    repository: &dyn UsageRepository,
) -> RefreshReport {
    RefreshReport {
        sources: adapters.iter().map(|adapter| refresh_one(adapter.as_ref(), repository)).collect(),
        // Quota failures never fail a refresh; the snapshots are a bonus.
        quotas: adapters
            .iter()
            .filter_map(|adapter| adapter.quotas().ok())
            .flatten()
            .collect(),
    }
}

fn refresh_one(adapter: &dyn SourceAdapter, repository: &dyn UsageRepository) -> SourceRefreshReport {
    let mut report = SourceRefreshReport::new(adapter.id());

    let sources = match adapter.discover() {
        Ok(sources) => sources,
        Err(AdapterError::NotImplemented { .. }) => {
            report.status = RefreshStatus::Planned;
            return report;
        }
        Err(error) => {
            report.status = RefreshStatus::Failed;
            report.note_failure(error.to_string());
            return report;
        }
    };

    report.sources_found = sources.len();
    for source in sources {
        let drafts = adapter
            .read(&source)
            .and_then(|input| adapter.parse(&input))
            .map_err(|error| error.to_string());
        let drafts = match drafts {
            Ok(drafts) => drafts,
            Err(failure) => {
                report.note_failure(failure);
                continue;
            }
        };
        report.drafts_parsed += drafts.len();

        match pipeline::import_drafts(repository, drafts) {
            Ok(import) => {
                report.inserted += import.inserted;
                report.duplicates_skipped += import.duplicates_skipped;
                report.rejected += import.rejected_count();
            }
            Err(error) => report.note_failure(error.to_string()),
        }
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::{DiscoveredSource, RawSourceInput, SourceFormat};
    use crate::domain::{SourceApp, SourceProvenance, TokenCounts, TokenField, UsageRecordDraft};
    use crate::repository::InMemoryUsageRepository;

    /// An adapter that reads nothing, so refresh tests never touch real logs.
    struct PlannedAdapter;

    impl SourceAdapter for PlannedAdapter {
        fn id(&self) -> &'static str {
            "planned"
        }
        fn version(&self) -> &'static str {
            "0.0.0"
        }
        fn source_app(&self) -> SourceApp {
            SourceApp::Cursor
        }
        fn discover(&self) -> Result<Vec<DiscoveredSource>, AdapterError> {
            Err(AdapterError::NotImplemented { adapter: "planned", capability: "discover its logs" })
        }
        fn parse(&self, _input: &RawSourceInput) -> Result<Vec<UsageRecordDraft>, AdapterError> {
            Err(AdapterError::NotImplemented { adapter: "planned", capability: "parse its logs" })
        }
    }

    /// An adapter over two imaginary log files.
    struct FakeAdapter {
        fail_reads: bool,
    }

    impl SourceAdapter for FakeAdapter {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn version(&self) -> &'static str {
            "1.0.0"
        }
        fn source_app(&self) -> SourceApp {
            SourceApp::Codex
        }
        fn discover(&self) -> Result<Vec<DiscoveredSource>, AdapterError> {
            Ok(vec![
                DiscoveredSource {
                    source_ref: "one".to_string(),
                    label: "one".to_string(),
                    format: SourceFormat::Jsonl,
                },
                DiscoveredSource {
                    source_ref: "two".to_string(),
                    label: "two".to_string(),
                    format: SourceFormat::Jsonl,
                },
            ])
        }
        fn read(&self, source: &DiscoveredSource) -> Result<RawSourceInput, AdapterError> {
            if self.fail_reads && source.source_ref == "two" {
                return Err(AdapterError::Unreadable {
                    adapter: "fake",
                    reason: "went away".to_string(),
                });
            }
            Ok(RawSourceInput::from_source(source, "ignored"))
        }
        fn parse(&self, input: &RawSourceInput) -> Result<Vec<UsageRecordDraft>, AdapterError> {
            let provenance = SourceProvenance {
                adapter_id: "fake".to_string(),
                adapter_version: "1.0.0".to_string(),
                source_ref: input.source_ref.clone(),
            };
            Ok(vec![UsageRecordDraft::new(SourceApp::Codex, provenance)
                .with_raw_timestamp("2026-07-29T10:00:00Z")
                .with_source_event_id(format!("event-{}", input.source_ref.clone().unwrap()))
                .with_tokens(TokenCounts {
                    input: TokenField::exact(10),
                    ..TokenCounts::default()
                })])
        }
    }

    fn boxed(adapter: impl SourceAdapter + 'static) -> Box<dyn SourceAdapter> {
        Box::new(adapter)
    }

    #[test]
    fn imports_from_every_readable_adapter_and_marks_the_rest_planned() {
        let repository = InMemoryUsageRepository::new();
        let report = refresh_with(
            &[boxed(FakeAdapter { fail_reads: false }), boxed(PlannedAdapter)],
            &repository,
        );

        assert_eq!(report.sources.len(), 2);
        let fake = &report.sources[0];
        assert_eq!(fake.status, RefreshStatus::Imported);
        assert_eq!(fake.sources_found, 2);
        assert_eq!(fake.drafts_parsed, 2);
        assert_eq!(fake.inserted, 2);
        let planned = &report.sources[1];
        assert_eq!(planned.status, RefreshStatus::Planned);
        assert_eq!(repository.count().unwrap(), 2);
    }

    #[test]
    fn a_failing_source_does_not_stop_the_others() {
        let repository = InMemoryUsageRepository::new();
        let report =
            refresh_with(&[boxed(FakeAdapter { fail_reads: true })], &repository);

        let fake = &report.sources[0];
        assert_eq!(fake.status, RefreshStatus::Imported);
        assert_eq!(fake.sources_found, 2);
        assert_eq!(fake.sources_failed, 1);
        assert_eq!(fake.inserted, 1);
        assert_eq!(fake.failures.len(), 1);
    }

    #[test]
    fn refreshing_twice_only_counts_duplicates() {
        let repository = InMemoryUsageRepository::new();
        refresh_with(&[boxed(FakeAdapter { fail_reads: false })], &repository);
        let second = refresh_with(&[boxed(FakeAdapter { fail_reads: false })], &repository);

        assert_eq!(second.total_inserted(), 0);
        assert_eq!(second.sources[0].duplicates_skipped, 2);
        assert_eq!(repository.count().unwrap(), 2);
    }

    #[test]
    fn collects_quotas_from_adapters_that_report_them() {
        struct QuotaAdapter;

        impl SourceAdapter for QuotaAdapter {
            fn id(&self) -> &'static str {
                "quota-fake"
            }
            fn version(&self) -> &'static str {
                "1.0.0"
            }
            fn source_app(&self) -> SourceApp {
                SourceApp::Codex
            }
            fn discover(&self) -> Result<Vec<DiscoveredSource>, AdapterError> {
                Ok(vec![])
            }
            fn parse(&self, _input: &RawSourceInput) -> Result<Vec<UsageRecordDraft>, AdapterError> {
                Ok(vec![])
            }
            fn quotas(&self) -> Result<Vec<crate::domain::UsageQuota>, AdapterError> {
                Ok(vec![crate::domain::UsageQuota {
                    source_app: SourceApp::Codex,
                    label: None,
                    window_minutes: 10080,
                    used_percent_tenths: 930,
                    resets_at: chrono::DateTime::from_timestamp(1_785_264_899, 0).unwrap(),
                    observed_at: chrono::DateTime::from_timestamp(1_785_200_000, 0).unwrap(),
                }])
            }
        }

        let repository = InMemoryUsageRepository::new();
        let report = refresh_with(
            &[boxed(QuotaAdapter), boxed(PlannedAdapter)],
            &repository,
        );

        assert_eq!(report.quotas.len(), 1);
        assert_eq!(report.quotas[0].used_percent_tenths, 930);
    }
}
