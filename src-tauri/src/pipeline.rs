//! The import pipeline.
//!
//! One stage of the chain the plan specifies:
//!
//! ```text
//! coordinator → source adapter → UsageRecordDraft → normalizer/validator
//!             → repository transaction → committed revision → React
//! ```
//!
//! Only the normalizing middle lives here. Reading belongs to the adapters,
//! *when* to read and *when* to commit belongs to `crate::refresh`, and
//! presentation belongs to React. In particular this module no longer touches
//! the repository: writing is a transaction the coordinator assembles, so a
//! batch of records can commit together with the checkpoint that accounts for
//! them.

use serde::{Deserialize, Serialize};

use crate::domain::{UsageRecord, UsageRecordDraft};
use crate::normalize::{self, RejectedDraft};

/// The outcome of normalizing one source's delta, reported in full: partial
/// success is the expected case when a log contains entries this version
/// cannot make sense of.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizeReport {
    pub accepted: usize,
    pub rejected: Vec<RejectedDraft>,
}

impl NormalizeReport {
    pub fn rejected_count(&self) -> usize {
        self.rejected.len()
    }
}

/// Validate drafts, keeping what passes and reporting what does not.
///
/// Rejected drafts are reported, not raised: one malformed entry must never
/// discard the entries around it.
pub fn normalize_drafts(drafts: Vec<UsageRecordDraft>) -> (Vec<UsageRecord>, NormalizeReport) {
    let batch = normalize::normalize_batch(drafts);
    let report = NormalizeReport {
        accepted: batch.records.len(),
        rejected: batch.rejected,
    };
    (batch.records, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        FieldQuality, SourceApp, SourceProvenance, TokenCounts, TokenField, UsageRecordDraft,
    };

    fn draft(timestamp: &str) -> UsageRecordDraft {
        UsageRecordDraft::new(
            SourceApp::Cursor,
            SourceProvenance {
                adapter_id: "cursor".to_string(),
                adapter_version: "0.1.0".to_string(),
                source_ref: None,
            },
        )
        .with_raw_timestamp(timestamp)
        .with_tokens(TokenCounts {
            input: TokenField::exact(10),
            ..TokenCounts::default()
        })
    }

    #[test]
    fn keeps_valid_drafts_and_reports_the_rest() {
        let mut malformed = draft("2026-07-29T10:00:00Z");
        malformed.tokens.output = TokenField {
            value: None,
            quality: FieldQuality::Exact,
        };

        let (records, report) = normalize_drafts(vec![
            draft("2026-07-29T10:00:00Z"),
            malformed,
            draft("2026-07-29T11:00:00Z"),
        ]);

        assert_eq!(records.len(), 2);
        assert_eq!(report.accepted, 2);
        assert_eq!(report.rejected_count(), 1);
    }

    #[test]
    fn the_same_drafts_normalize_to_the_same_identities() {
        let drafts = || vec![draft("2026-07-29T10:00:00Z"), draft("2026-07-29T11:00:00Z")];
        let keys = |records: Vec<crate::domain::UsageRecord>| {
            records
                .into_iter()
                .map(|record| record.dedupe_key)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            keys(normalize_drafts(drafts()).0),
            keys(normalize_drafts(drafts()).0)
        );
    }
}
