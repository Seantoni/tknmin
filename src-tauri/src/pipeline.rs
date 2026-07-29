//! The import pipeline.
//!
//! Joins the stages the plan specifies:
//!
//! ```text
//! discovery → source adapter → UsageRecordDraft → normalizer/validator
//!           → repository → Tauri command → React
//! ```
//!
//! Only the middle of that chain lives here. Discovery and parsing belong to
//! the adapters; presentation belongs to React.

use serde::{Deserialize, Serialize};

use crate::domain::UsageRecordDraft;
use crate::normalize::{self, RejectedDraft};
use crate::repository::{RepositoryError, UsageRepository};

/// The outcome of one import, reported to the interface in full: partial
/// success is the expected case when a log contains entries this version
/// cannot make sense of.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub inserted: usize,
    pub duplicates_skipped: usize,
    pub rejected: Vec<RejectedDraft>,
}

impl ImportReport {
    pub fn rejected_count(&self) -> usize {
        self.rejected.len()
    }
}

/// Normalize drafts and store whatever passes validation.
///
/// Rejected drafts are reported, not raised: one malformed entry must never
/// discard the entries around it.
pub fn import_drafts(
    repository: &dyn UsageRepository,
    drafts: Vec<UsageRecordDraft>,
) -> Result<ImportReport, RepositoryError> {
    let batch = normalize::normalize_batch(drafts);
    let stored = repository.insert_batch(batch.records)?;

    Ok(ImportReport {
        inserted: stored.inserted,
        duplicates_skipped: stored.duplicates_skipped,
        rejected: batch.rejected,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        FieldQuality, SourceApp, SourceProvenance, TokenCounts, TokenField, UsageRecordDraft,
    };
    use crate::repository::InMemoryUsageRepository;

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
        .with_tokens(TokenCounts { input: TokenField::exact(10), ..TokenCounts::default() })
    }

    #[test]
    fn stores_valid_drafts_and_reports_the_rest() {
        let repository = InMemoryUsageRepository::new();
        let mut malformed = draft("2026-07-29T10:00:00Z");
        malformed.tokens.output = TokenField { value: None, quality: FieldQuality::Exact };

        let report = import_drafts(
            &repository,
            vec![draft("2026-07-29T10:00:00Z"), malformed, draft("2026-07-29T11:00:00Z")],
        )
        .unwrap();

        assert_eq!(report.inserted, 2);
        assert_eq!(report.rejected_count(), 1);
        assert_eq!(repository.count().unwrap(), 2);
    }

    #[test]
    fn re_importing_the_same_drafts_changes_nothing() {
        let repository = InMemoryUsageRepository::new();
        let drafts = || vec![draft("2026-07-29T10:00:00Z"), draft("2026-07-29T11:00:00Z")];

        import_drafts(&repository, drafts()).unwrap();
        let second = import_drafts(&repository, drafts()).unwrap();

        assert_eq!(second.inserted, 0);
        assert_eq!(second.duplicates_skipped, 2);
        assert_eq!(repository.count().unwrap(), 2);
    }
}
