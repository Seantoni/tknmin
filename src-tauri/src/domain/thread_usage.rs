//! Per-thread usage: what one conversation consumed.
//!
//! A thread is the closest thing the sources have to a task: a Claude Code
//! session, a Cursor chat, a Codex rollout. Every adapter already stamps it
//! on its records as `session_id`, so grouping here never reads a source — it
//! folds what is already stored.
//!
//! Records with no session id are not dropped silently: they are counted as
//! unattributed, so the interface can say what the breakdown leaves out.
//!
//! Labels are deliberately *not* built here. A thread's identity is an opaque
//! session id, and the privacy rule that keeps prompts and paths out of the
//! store leaves this module only metadata to describe it with — source,
//! project, models, and time span. The interface composes those into a label.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::record::UsageRecord;
use super::source::SourceApp;
use super::summary::{SummaryQuery, UsageTotals};

/// Parameters for the thread breakdown: the dashboard's filter, plus a cap so
/// a busy store cannot return an unbounded list. Mirrors [`super::summary::RecentQuery`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ThreadUsageQuery {
    pub filter: SummaryQuery,
    pub limit: usize,
}

impl ThreadUsageQuery {
    pub const DEFAULT_LIMIT: usize = 50;
    pub const MAX_LIMIT: usize = 500;

    /// Clamps the caller's limit into the supported range.
    pub fn effective_limit(&self) -> usize {
        self.limit.clamp(1, Self::MAX_LIMIT)
    }
}

impl Default for ThreadUsageQuery {
    fn default() -> Self {
        Self {
            filter: SummaryQuery::default(),
            limit: Self::DEFAULT_LIMIT,
        }
    }
}

/// One conversation's usage. Identity is the (source, session) pair: session
/// ids are only unique inside their own source, so the key carries both.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    /// Stable key: `{source}:{session_id}`.
    pub key: String,
    pub session_id: String,
    pub source_app: SourceApp,
    /// Where the thread's most recent dated record ran, when any record said.
    pub project: Option<String>,
    /// Every model the thread used, sorted — a session that switched models
    /// reports all of them.
    pub models: Vec<String>,
    pub totals: UsageTotals,
    pub first_event_at: Option<DateTime<Utc>>,
    pub last_event_at: Option<DateTime<Utc>>,
}

/// The whole breakdown for one filtered set of records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadUsageReport {
    pub generated_at: DateTime<Utc>,
    /// Grand totals over every thread-attributed record, whether or not its
    /// thread made the capped list, so a row's share is against one whole.
    pub totals: UsageTotals,
    pub threads: Vec<ThreadSummary>,
    /// Threads before the limit was applied, so the interface can say
    /// "showing 50 of 312" rather than implying the list is complete.
    pub thread_count: usize,
    /// Records the filter kept but no thread claims.
    pub unattributed_records: usize,
    /// Records a date-bounded filter excluded for having no resolved timestamp.
    pub undated_records_excluded: usize,
}

/// Fold already-filtered records into one row per thread.
///
/// The caller supplies records that already passed its query, the same
/// contract [`crate::repository::summarize::summarize`] has, so both backends
/// share this fold exactly as they share that one.
pub fn group_threads<'a>(
    records: impl Iterator<Item = &'a UsageRecord>,
    generated_at: DateTime<Utc>,
    undated_records_excluded: usize,
    limit: usize,
) -> ThreadUsageReport {
    struct Fold {
        source_app: SourceApp,
        session_id: String,
        project: Option<String>,
        /// Event time of the record the project came from, so a later record
        /// from another directory supersedes it regardless of arrival order.
        project_at: Option<DateTime<Utc>>,
        models: BTreeSet<String>,
        totals: UsageTotals,
        first_event_at: Option<DateTime<Utc>>,
        last_event_at: Option<DateTime<Utc>>,
    }

    let mut totals = UsageTotals::default();
    let mut unattributed_records = 0;
    let mut folds: BTreeMap<String, Fold> = BTreeMap::new();

    for record in records {
        let Some(session_id) = record.session_id.as_deref() else {
            unattributed_records += 1;
            continue;
        };
        totals.add_record(record);

        let key = format!("{}:{session_id}", record.source_app.as_str());
        let fold = folds.entry(key).or_insert_with(|| Fold {
            source_app: record.source_app,
            session_id: session_id.to_string(),
            project: None,
            project_at: None,
            models: BTreeSet::new(),
            totals: UsageTotals::default(),
            first_event_at: None,
            last_event_at: None,
        });
        fold.totals.add_record(record);
        if let Some(model) = &record.model {
            fold.models.insert(model.clone());
        }
        match record.event_timestamp_utc {
            Some(event) => {
                fold.first_event_at =
                    Some(fold.first_event_at.map_or(event, |first| first.min(event)));
                fold.last_event_at =
                    Some(fold.last_event_at.map_or(event, |last| last.max(event)));
                // Records arrive unordered from the backends, so "the thread's
                // project" is decided by event time, not by position.
                if record.project.is_some() && fold.project_at.is_none_or(|at| event >= at) {
                    fold.project = record.project.clone();
                    fold.project_at = Some(event);
                }
            }
            None => {
                if fold.project.is_none() {
                    fold.project = record.project.clone();
                }
            }
        }
    }

    let thread_count = folds.len();
    let mut threads: Vec<ThreadSummary> = folds
        .into_iter()
        .map(|(key, fold)| ThreadSummary {
            key,
            session_id: fold.session_id,
            source_app: fold.source_app,
            project: fold.project,
            models: fold.models.into_iter().collect(),
            totals: fold.totals,
            first_event_at: fold.first_event_at,
            last_event_at: fold.last_event_at,
        })
        .collect();

    // Heaviest first, then most recently active, then key — the order never
    // depends on which backend supplied the records.
    threads.sort_by(|left, right| {
        right
            .totals
            .display_total
            .tokens
            .cmp(&left.totals.display_total.tokens)
            .then(right.last_event_at.cmp(&left.last_event_at))
            .then(left.key.cmp(&right.key))
    });
    threads.truncate(limit);

    ThreadUsageReport {
        generated_at,
        totals,
        threads,
        thread_count,
        unattributed_records,
        undated_records_excluded,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        CostInfo, DisplayTotal, SourceProvenance, TimestampInterpretation, TokenCounts, TokenField,
        TotalRule, NORMALIZATION_VERSION,
    };
    use chrono::TimeZone;

    fn record(
        source_app: SourceApp,
        session_id: Option<&str>,
        tokens: u64,
        day: u32,
        project: Option<&str>,
        model: Option<&str>,
    ) -> UsageRecord {
        let event = Utc.with_ymd_and_hms(2026, 7, day, 12, 0, 0).unwrap();
        UsageRecord {
            normalization_version: NORMALIZATION_VERSION,
            id: format!("{source_app:?}-{session_id:?}-{day}-{tokens}"),
            raw_timestamp: None,
            event_timestamp_utc: Some(event),
            timestamp_interpretation: TimestampInterpretation::ExplicitOffset,
            source_app,
            source_event_id: None,
            dedupe_key: String::new(),
            dedupe_algorithm_version: 1,
            content_hash: String::new(),
            content_hash_version: 1,
            provider: None,
            model: model.map(str::to_string),
            tokens: TokenCounts {
                input: TokenField::exact(tokens),
                output: TokenField::exact(0),
                cached_input: TokenField::exact(0),
                reasoning: TokenField::unknown(),
            },
            reported_total_tokens: None,
            display_total: Some(DisplayTotal {
                tokens,
                rule: TotalRule::InputPlusOutput,
            }),
            project: project.map(str::to_string),
            session_id: session_id.map(str::to_string),
            cost: CostInfo::not_available(),
            imported_at: event,
            provenance: SourceProvenance {
                adapter_id: "test".to_string(),
                adapter_version: "0".to_string(),
                source_ref: None,
            },
        }
    }

    fn group(records: &[UsageRecord], limit: usize) -> ThreadUsageReport {
        group_threads(
            records.iter(),
            Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
            0,
            limit,
        )
    }

    #[test]
    fn folds_records_into_one_row_per_thread() {
        let records = vec![
            record(SourceApp::ClaudeCode, Some("s1"), 100, 29, Some("app"), Some("opus")),
            record(SourceApp::ClaudeCode, Some("s1"), 50, 30, Some("app"), Some("opus")),
            record(SourceApp::ClaudeCode, Some("s2"), 25, 30, None, None),
        ];

        let report = group(&records, 50);

        assert_eq!(report.thread_count, 2);
        assert_eq!(report.unattributed_records, 0);
        assert_eq!(report.totals.display_total.tokens, 175);

        let first = &report.threads[0];
        assert_eq!(first.key, "claude_code:s1");
        assert_eq!(first.session_id, "s1");
        assert_eq!(first.totals.display_total.tokens, 150);
        assert_eq!(first.totals.record_count, 2);
        assert_eq!(first.project.as_deref(), Some("app"));
        assert_eq!(first.models, vec!["opus".to_string()]);
        assert_eq!(
            first.first_event_at,
            Some(Utc.with_ymd_and_hms(2026, 7, 29, 12, 0, 0).unwrap())
        );
        assert_eq!(
            first.last_event_at,
            Some(Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap())
        );
    }

    #[test]
    fn the_same_session_id_in_two_sources_is_two_threads() {
        let records = vec![
            record(SourceApp::ClaudeCode, Some("s1"), 100, 29, None, None),
            record(SourceApp::Cursor, Some("s1"), 40, 29, None, None),
        ];

        let report = group(&records, 50);

        assert_eq!(report.thread_count, 2);
        assert!(report
            .threads
            .iter()
            .any(|thread| thread.key == "claude_code:s1"));
        assert!(report
            .threads
            .iter()
            .any(|thread| thread.key == "cursor:s1"));
    }

    #[test]
    fn records_without_a_session_are_counted_not_grouped() {
        let records = vec![
            record(SourceApp::Codex, None, 100, 29, None, None),
            record(SourceApp::Codex, Some("r1"), 40, 29, None, None),
        ];

        let report = group(&records, 50);

        assert_eq!(report.thread_count, 1);
        assert_eq!(report.unattributed_records, 1);
        // The unattributed record stays out of the thread totals too, so a
        // share is always against what the rows actually sum to.
        assert_eq!(report.totals.display_total.tokens, 40);
    }

    #[test]
    fn the_project_comes_from_the_latest_dated_record() {
        // Out of arrival order on purpose: the backends do not guarantee one.
        let records = vec![
            record(SourceApp::ClaudeCode, Some("s1"), 10, 30, Some("new"), None),
            record(SourceApp::ClaudeCode, Some("s1"), 10, 29, Some("old"), None),
        ];

        let report = group(&records, 50);

        assert_eq!(report.threads[0].project.as_deref(), Some("new"));
    }

    #[test]
    fn a_thread_that_switched_models_reports_both() {
        let records = vec![
            record(SourceApp::Cursor, Some("c1"), 10, 29, None, Some("gpt-5")),
            record(SourceApp::Cursor, Some("c1"), 10, 30, None, Some("composer")),
        ];

        let report = group(&records, 50);

        assert_eq!(
            report.threads[0].models,
            vec!["composer".to_string(), "gpt-5".to_string()]
        );
    }

    #[test]
    fn heaviest_first_then_the_limit_caps_the_list_not_the_count() {
        let records = vec![
            record(SourceApp::Codex, Some("small"), 10, 29, None, None),
            record(SourceApp::Codex, Some("big"), 90, 29, None, None),
            record(SourceApp::Codex, Some("mid"), 40, 29, None, None),
        ];

        let report = group(&records, 2);

        assert_eq!(report.thread_count, 3);
        assert_eq!(report.threads.len(), 2);
        assert_eq!(report.threads[0].session_id, "big");
        assert_eq!(report.threads[1].session_id, "mid");
        // The grand total covers the thread the cap hid as well.
        assert_eq!(report.totals.display_total.tokens, 140);
    }

    #[test]
    fn query_limit_clamps_like_the_recent_list() {
        assert_eq!(
            ThreadUsageQuery {
                limit: 0,
                ..ThreadUsageQuery::default()
            }
            .effective_limit(),
            1
        );
        assert_eq!(
            ThreadUsageQuery {
                limit: 10_000,
                ..ThreadUsageQuery::default()
            }
            .effective_limit(),
            ThreadUsageQuery::MAX_LIMIT
        );
    }
}
