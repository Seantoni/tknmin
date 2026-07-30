//! Deterministic fake usage data.
//!
//! The MVP proves the dashboard before any log is read, so the application
//! starts with this dataset instead of an empty store. Nothing here touches
//! the filesystem or the clock: the same records, identifiers, and totals
//! appear on every launch.
//!
//! The set deliberately includes awkward cases the real sources will produce —
//! an unknown provider and model, a timestamp without a zone, estimated rather
//! than exact counts, a source-reported total, and records with no cost — so
//! the interface is built against them from the start.
//!
//! Phase 6 replaces this with real imports; deleting this module and the
//! `AppState::with_fake_data` constructor is all that removal requires.

use chrono::{DateTime, TimeZone, Utc};

use crate::domain::{
    CostCalculationStatus, CostDraft, FieldQuality, Money, SourceApp, SourceProvenance,
    TokenCounts, TokenField, UsageRecord, UsageRecordDraft,
};
use crate::normalize;

/// Bumped when the dataset changes, so a stale build is recognizable.
pub const FIXTURE_VERSION: &str = "0.1.0";

/// Provenance identifying these records as generated rather than imported.
/// It is deliberately not a real adapter identifier.
pub const FIXTURE_ADAPTER_ID: &str = "fixture";

/// Costs are expressed in millionths of a unit, the scale token pricing needs.
const COST_EXPONENT: u8 = 6;
const CURRENCY: &str = "USD";
const PRICING_VERSION: &str = "fake-2026-07";

/// One row of the dataset.
struct Entry {
    source: SourceApp,
    event_id: &'static str,
    timestamp: &'static str,
    provider: Option<&'static str>,
    model: Option<&'static str>,
    input: Option<u64>,
    output: Option<u64>,
    cached_input: Option<u64>,
    reasoning: Option<u64>,
    reported_total: Option<u64>,
    project: Option<&'static str>,
    session: Option<&'static str>,
    /// Cost in millionths of a US dollar.
    cost_micros: Option<i64>,
    /// Applied to every token count this entry reports.
    quality: FieldQuality,
    /// Whether the source reported the cost itself or it was priced locally.
    cost_status: CostCalculationStatus,
}

const EXACT: FieldQuality = FieldQuality::Exact;
const REPORTED: CostCalculationStatus = CostCalculationStatus::ReportedBySource;
const PRICED: CostCalculationStatus = CostCalculationStatus::ComputedFromPricing;

const ENTRIES: &[Entry] = &[
    // Cursor — reports cost directly, and one entry writes a zone-less timestamp.
    Entry {
        source: SourceApp::Cursor,
        event_id: "fake-cursor-001",
        timestamp: "2026-07-21T09:12:04Z",
        provider: Some("anthropic"),
        model: Some("claude-sonnet-5"),
        input: Some(4_820),
        output: Some(612),
        cached_input: Some(1_200),
        reasoning: None,
        reported_total: None,
        project: Some("tokens"),
        session: Some("cur-1"),
        cost_micros: Some(24_600),
        quality: EXACT,
        cost_status: REPORTED,
    },
    Entry {
        source: SourceApp::Cursor,
        event_id: "fake-cursor-002",
        timestamp: "2026-07-21T14:41:19Z",
        provider: Some("anthropic"),
        model: Some("claude-sonnet-5"),
        input: Some(9_130),
        output: Some(1_044),
        cached_input: Some(3_400),
        reasoning: None,
        reported_total: None,
        project: Some("tokens"),
        session: Some("cur-1"),
        cost_micros: Some(47_800),
        quality: EXACT,
        cost_status: REPORTED,
    },
    Entry {
        source: SourceApp::Cursor,
        event_id: "fake-cursor-003",
        timestamp: "2026-07-23T10:05:41Z",
        provider: Some("anthropic"),
        model: Some("claude-opus-5"),
        input: Some(15_240),
        output: Some(2_310),
        cached_input: Some(5_120),
        reasoning: None,
        reported_total: None,
        project: Some("tokens"),
        session: Some("cur-2"),
        cost_micros: Some(210_400),
        quality: EXACT,
        cost_status: REPORTED,
    },
    Entry {
        source: SourceApp::Cursor,
        event_id: "fake-cursor-004",
        // No zone: normalization marks this record as assumed UTC.
        timestamp: "2026-07-24 09:15:00",
        provider: Some("openai"),
        model: Some("gpt-5"),
        input: Some(6_410),
        output: Some(890),
        cached_input: None,
        reasoning: None,
        reported_total: None,
        project: Some("deva"),
        session: Some("cur-3"),
        cost_micros: Some(31_200),
        quality: EXACT,
        cost_status: REPORTED,
    },
    Entry {
        source: SourceApp::Cursor,
        event_id: "fake-cursor-005",
        timestamp: "2026-07-27T16:22:58Z",
        provider: Some("anthropic"),
        model: Some("claude-opus-5"),
        input: Some(22_180),
        output: Some(3_402),
        cached_input: Some(9_800),
        reasoning: None,
        reported_total: None,
        project: Some("tokens"),
        session: Some("cur-4"),
        cost_micros: Some(318_700),
        quality: EXACT,
        cost_status: REPORTED,
    },
    Entry {
        source: SourceApp::Cursor,
        event_id: "fake-cursor-006",
        timestamp: "2026-07-28T08:44:12Z",
        // The source named the model but not who served it.
        provider: None,
        model: Some("claude-sonnet-5"),
        input: Some(3_120),
        output: Some(401),
        cached_input: None,
        reasoning: None,
        reported_total: None,
        project: Some("tokens"),
        session: Some("cur-5"),
        cost_micros: None,
        quality: EXACT,
        cost_status: CostCalculationStatus::NotAvailable,
    },
    // Claude Code — cost priced locally, cache reads reported separately.
    Entry {
        source: SourceApp::ClaudeCode,
        event_id: "fake-claude-001",
        timestamp: "2026-07-20T11:02:33Z",
        provider: Some("anthropic"),
        model: Some("claude-opus-5"),
        input: Some(18_420),
        output: Some(2_140),
        cached_input: Some(12_200),
        reasoning: None,
        reported_total: None,
        project: Some("tokens"),
        session: Some("cc-1"),
        cost_micros: Some(264_500),
        quality: EXACT,
        cost_status: PRICED,
    },
    Entry {
        source: SourceApp::ClaudeCode,
        event_id: "fake-claude-002",
        timestamp: "2026-07-22T12:18:07Z",
        provider: Some("anthropic"),
        model: Some("claude-opus-5"),
        input: Some(31_500),
        output: Some(4_180),
        cached_input: Some(21_400),
        reasoning: None,
        reported_total: None,
        project: Some("tokens"),
        session: Some("cc-1"),
        cost_micros: Some(471_200),
        quality: EXACT,
        cost_status: PRICED,
    },
    Entry {
        source: SourceApp::ClaudeCode,
        event_id: "fake-claude-003",
        timestamp: "2026-07-25T17:36:52Z",
        provider: Some("anthropic"),
        model: Some("claude-sonnet-5"),
        input: Some(12_780),
        output: Some(1_620),
        cached_input: Some(8_100),
        reasoning: None,
        reported_total: None,
        project: Some("deva"),
        session: Some("cc-2"),
        cost_micros: Some(88_300),
        quality: EXACT,
        cost_status: PRICED,
    },
    Entry {
        source: SourceApp::ClaudeCode,
        event_id: "fake-claude-004",
        timestamp: "2026-07-26T09:41:15Z",
        provider: Some("anthropic"),
        model: Some("claude-opus-5"),
        input: Some(41_220),
        output: Some(5_310),
        cached_input: Some(28_600),
        reasoning: None,
        reported_total: None,
        project: Some("tokens"),
        session: Some("cc-3"),
        cost_micros: Some(612_900),
        quality: EXACT,
        cost_status: PRICED,
    },
    Entry {
        source: SourceApp::ClaudeCode,
        event_id: "fake-claude-005",
        timestamp: "2026-07-28T13:07:44Z",
        provider: Some("anthropic"),
        model: Some("claude-haiku-4-5"),
        input: Some(5_210),
        output: Some(940),
        cached_input: Some(2_100),
        reasoning: None,
        reported_total: None,
        project: Some("deva"),
        session: Some("cc-4"),
        cost_micros: Some(9_800),
        quality: EXACT,
        cost_status: PRICED,
    },
    Entry {
        source: SourceApp::ClaudeCode,
        event_id: "fake-claude-006",
        timestamp: "2026-07-29T08:12:20Z",
        provider: Some("anthropic"),
        model: Some("claude-opus-5"),
        input: Some(27_640),
        output: Some(3_890),
        cached_input: Some(19_200),
        reasoning: None,
        reported_total: None,
        project: Some("tokens"),
        session: Some("cc-5"),
        cost_micros: Some(402_100),
        quality: EXACT,
        cost_status: PRICED,
    },
    // Codex — reports reasoning tokens, never cost.
    Entry {
        source: SourceApp::Codex,
        event_id: "fake-codex-001",
        timestamp: "2026-07-21T15:55:10Z",
        provider: Some("openai"),
        model: Some("gpt-5-codex"),
        input: Some(7_420),
        output: Some(1_180),
        cached_input: None,
        reasoning: Some(2_400),
        reported_total: None,
        project: Some("tokens"),
        session: Some("cdx-1"),
        cost_micros: None,
        quality: EXACT,
        cost_status: CostCalculationStatus::NotAvailable,
    },
    Entry {
        source: SourceApp::Codex,
        event_id: "fake-codex-002",
        timestamp: "2026-07-23T18:04:36Z",
        provider: Some("openai"),
        model: Some("gpt-5-codex"),
        input: Some(11_930),
        output: Some(1_640),
        cached_input: None,
        reasoning: Some(5_100),
        reported_total: None,
        project: Some("tokens"),
        session: Some("cdx-1"),
        cost_micros: None,
        quality: EXACT,
        cost_status: CostCalculationStatus::NotAvailable,
    },
    Entry {
        source: SourceApp::Codex,
        event_id: "fake-codex-003",
        timestamp: "2026-07-24T20:11:09Z",
        provider: Some("openai"),
        model: Some("gpt-5-codex"),
        input: Some(5_120),
        output: Some(760),
        cached_input: None,
        reasoning: Some(1_900),
        reported_total: None,
        project: Some("tokens"),
        session: Some("cdx-2"),
        cost_micros: None,
        quality: EXACT,
        cost_status: CostCalculationStatus::NotAvailable,
    },
    Entry {
        source: SourceApp::Codex,
        event_id: "fake-codex-004",
        timestamp: "2026-07-25T10:27:03Z",
        provider: Some("openai"),
        model: Some("o4-mini"),
        input: Some(3_180),
        output: Some(520),
        cached_input: None,
        reasoning: Some(1_450),
        reported_total: None,
        project: Some("deva"),
        session: Some("cdx-2"),
        cost_micros: None,
        // This source only estimated its counts.
        quality: FieldQuality::Estimated,
        cost_status: CostCalculationStatus::NotAvailable,
    },
    Entry {
        source: SourceApp::Codex,
        event_id: "fake-codex-005",
        timestamp: "2026-07-27T11:49:27Z",
        // Neither provider nor model was recorded.
        provider: None,
        model: None,
        input: Some(8_640),
        output: Some(1_210),
        cached_input: None,
        reasoning: Some(3_300),
        reported_total: None,
        project: None,
        session: Some("cdx-3"),
        cost_micros: None,
        quality: EXACT,
        cost_status: CostCalculationStatus::NotAvailable,
    },
    Entry {
        source: SourceApp::Codex,
        event_id: "fake-codex-006",
        timestamp: "2026-07-29T09:33:51Z",
        provider: Some("openai"),
        model: Some("gpt-5-codex"),
        input: Some(14_210),
        output: Some(2_050),
        cached_input: None,
        reasoning: Some(6_400),
        // The source supplied its own total, which takes precedence.
        reported_total: Some(22_660),
        project: Some("tokens"),
        session: Some("cdx-4"),
        cost_micros: None,
        quality: EXACT,
        cost_status: CostCalculationStatus::NotAvailable,
    },
];

/// The import timestamp stamped on every fake record.
///
/// Fixed rather than "now" so two launches produce byte-identical records.
fn fixture_imported_at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0)
        .single()
        .expect("fixture import time is valid")
}

fn provenance() -> SourceProvenance {
    SourceProvenance {
        adapter_id: FIXTURE_ADAPTER_ID.to_string(),
        adapter_version: FIXTURE_VERSION.to_string(),
        source_ref: Some("fake-dataset".to_string()),
    }
}

fn token_field(value: Option<u64>, quality: FieldQuality) -> TokenField {
    match value {
        Some(value) => TokenField {
            value: Some(value),
            quality,
        },
        None => TokenField::unknown(),
    }
}

impl Entry {
    fn to_draft(&self) -> UsageRecordDraft {
        let mut draft = UsageRecordDraft::new(self.source, provenance())
            .with_raw_timestamp(self.timestamp)
            .with_source_event_id(self.event_id)
            .with_tokens(TokenCounts {
                input: token_field(self.input, self.quality),
                output: token_field(self.output, self.quality),
                cached_input: token_field(self.cached_input, self.quality),
                reasoning: token_field(self.reasoning, self.quality),
            })
            .with_session(self.project, self.session);

        draft.provider = self.provider.map(str::to_string);
        draft.model = self.model.map(str::to_string);
        draft.reported_total_tokens = self.reported_total;

        if let Some(micros) = self.cost_micros {
            draft = draft.with_cost(CostDraft {
                amount: Some(
                    Money::new(micros, CURRENCY, COST_EXPONENT).expect("fixture cost is valid"),
                ),
                status: self.cost_status,
                pricing_version: match self.cost_status {
                    CostCalculationStatus::ComputedFromPricing => Some(PRICING_VERSION.to_string()),
                    _ => None,
                },
            });
        }

        draft
    }
}

/// The dataset as adapter-shaped drafts.
pub fn fake_drafts() -> Vec<UsageRecordDraft> {
    ENTRIES.iter().map(Entry::to_draft).collect()
}

/// The dataset as normalized records, ready for the repository.
///
/// The fixtures are written to satisfy every validation rule, so nothing is
/// rejected here; if that ever stops holding, the tests below fail rather than
/// the dataset silently shrinking.
pub fn fake_records() -> Vec<UsageRecord> {
    normalize::normalize_batch_at(fake_drafts(), fixture_imported_at()).records
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{RecentQuery, SummaryQuery, TimestampInterpretation, TotalRule};
    use crate::repository::{InMemoryUsageRepository, SourceTransaction, UsageReader, UsageWriter};

    fn seed(repository: &InMemoryUsageRepository) -> crate::repository::CommitCounts {
        repository
            .commit(SourceTransaction {
                records: fake_records(),
                ..SourceTransaction::new(
                    crate::domain::SourceApp::Cursor,
                    "fixture",
                    chrono::Utc::now(),
                )
            })
            .unwrap()
            .counts
    }

    #[test]
    fn every_fixture_normalizes() {
        let batch = normalize::normalize_batch_at(fake_drafts(), fixture_imported_at());
        assert!(batch.rejected.is_empty(), "rejected: {:?}", batch.rejected);
        assert_eq!(batch.records.len(), ENTRIES.len());
    }

    #[test]
    fn the_dataset_is_identical_on_every_call() {
        assert_eq!(fake_records(), fake_records());
    }

    #[test]
    fn event_ids_are_unique_so_nothing_deduplicates_away() {
        let repository = InMemoryUsageRepository::new();
        let first = seed(&repository);
        assert_eq!(first.inserted, ENTRIES.len());
        assert_eq!(first.unchanged, 0);

        // Seeding twice must not double the dashboard's totals, and the
        // identical content must be recognised as unchanged rather than
        // rewritten.
        let second = seed(&repository);
        assert_eq!(second.inserted, 0);
        assert_eq!(second.unchanged, ENTRIES.len());
    }

    #[test]
    fn totals_match_the_records_they_summarize() {
        let repository = InMemoryUsageRepository::with_records(fake_records());
        let summary = repository.summary(&SummaryQuery::default()).unwrap();

        let expected_input: u64 = ENTRIES.iter().filter_map(|entry| entry.input).sum();
        let expected_output: u64 = ENTRIES.iter().filter_map(|entry| entry.output).sum();

        assert_eq!(summary.totals.record_count, ENTRIES.len());
        assert_eq!(summary.totals.input.tokens, expected_input);
        assert_eq!(summary.totals.output.tokens, expected_output);
        assert_eq!(summary.totals.input.unknown_records, 0);
        // Only Codex reports reasoning, so that total is openly partial.
        assert!(summary.totals.reasoning.is_partial());
    }

    #[test]
    fn cost_totals_stay_in_one_currency() {
        let repository = InMemoryUsageRepository::with_records(fake_records());
        let summary = repository.summary(&SummaryQuery::default()).unwrap();

        let expected: i64 = ENTRIES.iter().filter_map(|entry| entry.cost_micros).sum();
        let priced = ENTRIES
            .iter()
            .filter(|entry| entry.cost_micros.is_some())
            .count();

        assert_eq!(summary.totals.cost.by_currency.len(), 1);
        let usd = &summary.totals.cost.by_currency[0];
        assert_eq!(
            usd.amount,
            Money::new(expected, CURRENCY, COST_EXPONENT).unwrap()
        );
        assert_eq!(usd.counted_records, priced);
        assert_eq!(
            summary.totals.cost.records_without_cost,
            ENTRIES.len() - priced
        );
    }

    #[test]
    fn covers_every_source_and_an_unknown_model() {
        let repository = InMemoryUsageRepository::with_records(fake_records());
        let summary = repository.summary(&SummaryQuery::default()).unwrap();

        assert_eq!(summary.by_source.len(), SourceApp::ALL.len());
        assert!(summary.by_model.iter().any(|group| group.key.is_none()));
    }

    #[test]
    fn exercises_the_awkward_cases_the_interface_must_handle() {
        let records = fake_records();

        assert!(records
            .iter()
            .any(|record| record.timestamp_interpretation == TimestampInterpretation::AssumedUtc));
        assert!(records.iter().any(|record| record.provider.is_none()));
        assert!(records
            .iter()
            .any(|record| record.tokens.input.quality == FieldQuality::Estimated));
        assert!(records.iter().any(|record| record
            .display_total
            .is_some_and(|total| total.rule == TotalRule::ReportedBySource)));
        assert!(records.iter().any(|record| record.cost.amount.is_none()));
    }

    #[test]
    fn the_recent_list_is_ordered_newest_first() {
        let repository = InMemoryUsageRepository::with_records(fake_records());
        let recent = repository.recent(&RecentQuery::default()).unwrap();

        assert_eq!(recent.len(), ENTRIES.len());
        assert_eq!(recent[0].source_event_id.as_deref(), Some("fake-codex-006"));
        assert!(recent
            .windows(2)
            .all(|pair| pair[0].event_timestamp_utc >= pair[1].event_timestamp_utc));
    }
}
