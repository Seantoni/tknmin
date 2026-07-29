//! Validates and canonicalizes adapter drafts into [`UsageRecord`]s.
//!
//! This is the only place a `UsageRecord` is created. Everything downstream —
//! the repository, the Tauri commands, React — can therefore assume the
//! invariants checked here already hold.

pub mod dedupe;
pub mod timestamp;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{
    CostCalculationStatus, CostDraft, CostInfo, DisplayTotal, Money, SourceApp, TokenCounts,
    TotalRule, UsageRecord, UsageRecordDraft, NORMALIZATION_VERSION,
};

pub use dedupe::DEDUPE_ALGORITHM_VERSION;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NormalizationError {
    #[error("token field `{field}` has no value but is marked as {quality:?}")]
    InconsistentTokenQuality { field: &'static str, quality: crate::domain::FieldQuality },
    #[error("invalid cost: {0}")]
    InvalidCost(String),
    #[error("cost is marked as computed from pricing but no pricing version was supplied")]
    MissingPricingVersion,
    #[error("cost carries an amount but is marked as unavailable")]
    ContradictoryCostStatus,
}

/// One draft that could not be normalized, kept so a single malformed entry
/// never aborts an import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RejectedDraft {
    /// Position of the draft within the batch it arrived in.
    pub index: usize,
    pub source_app: SourceApp,
    pub reason: String,
}

/// The result of normalizing a batch: what survived, and what did not.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedBatch {
    pub records: Vec<UsageRecord>,
    pub rejected: Vec<RejectedDraft>,
}

/// Normalize a draft, stamping it with the current import time.
pub fn normalize(draft: UsageRecordDraft) -> Result<UsageRecord, NormalizationError> {
    normalize_at(draft, Utc::now())
}

/// Normalize a draft with an explicit import timestamp.
///
/// Taking the clock as a parameter keeps normalization a pure function, which
/// is what makes the output reproducible in tests and across imports.
pub fn normalize_at(
    draft: UsageRecordDraft,
    imported_at: DateTime<Utc>,
) -> Result<UsageRecord, NormalizationError> {
    validate_tokens(&draft.tokens)?;
    let cost = normalize_cost(draft.cost.clone())?;

    let dedupe_key = dedupe::dedupe_key(&draft);
    let id = dedupe::record_id(draft.source_app.as_str(), &dedupe_key);

    let raw_timestamp = clean(draft.raw_timestamp.as_deref());
    let (event_timestamp_utc, timestamp_interpretation) =
        timestamp::parse_source_timestamp(raw_timestamp.as_deref());

    let display_total = compute_display_total(&draft.tokens, draft.reported_total_tokens);

    Ok(UsageRecord {
        normalization_version: NORMALIZATION_VERSION,
        id,
        raw_timestamp,
        event_timestamp_utc,
        timestamp_interpretation,
        source_app: draft.source_app,
        source_event_id: clean(draft.source_event_id.as_deref()),
        dedupe_key,
        dedupe_algorithm_version: DEDUPE_ALGORITHM_VERSION,
        provider: clean(draft.provider.as_deref()),
        model: clean(draft.model.as_deref()),
        tokens: draft.tokens,
        reported_total_tokens: draft.reported_total_tokens,
        display_total,
        project: clean(draft.project.as_deref()),
        session_id: clean(draft.session_id.as_deref()),
        cost,
        imported_at,
        provenance: draft.provenance,
    })
}

/// Normalize a whole batch, collecting failures instead of stopping at the
/// first one.
pub fn normalize_batch(drafts: Vec<UsageRecordDraft>) -> NormalizedBatch {
    normalize_batch_at(drafts, Utc::now())
}

pub fn normalize_batch_at(drafts: Vec<UsageRecordDraft>, imported_at: DateTime<Utc>) -> NormalizedBatch {
    let mut batch = NormalizedBatch::default();
    for (index, draft) in drafts.into_iter().enumerate() {
        let source_app = draft.source_app;
        match normalize_at(draft, imported_at) {
            Ok(record) => batch.records.push(record),
            Err(error) => batch.rejected.push(RejectedDraft {
                index,
                source_app,
                reason: error.to_string(),
            }),
        }
    }
    batch
}

/// The display total the dashboard shows for one record.
///
/// A source-reported total wins, because only the source knows how it counts.
/// Otherwise input and output are summed, plus reasoning when it is reported
/// separately. Cached input is excluded: sources generally report it as a
/// component of input, so adding it would double count.
fn compute_display_total(tokens: &TokenCounts, reported_total: Option<u64>) -> Option<DisplayTotal> {
    if let Some(tokens) = reported_total {
        return Some(DisplayTotal { tokens, rule: TotalRule::ReportedBySource });
    }

    let input = tokens.input.countable();
    let output = tokens.output.countable();
    if input.is_none() && output.is_none() {
        return None;
    }

    let base = input.unwrap_or(0).saturating_add(output.unwrap_or(0));
    match tokens.reasoning.countable() {
        Some(reasoning) => Some(DisplayTotal {
            tokens: base.saturating_add(reasoning),
            rule: TotalRule::InputPlusOutputPlusReasoning,
        }),
        None => Some(DisplayTotal { tokens: base, rule: TotalRule::InputPlusOutput }),
    }
}

fn validate_tokens(tokens: &TokenCounts) -> Result<(), NormalizationError> {
    for (name, field) in tokens.fields() {
        if !field.has_consistent_quality() {
            return Err(NormalizationError::InconsistentTokenQuality {
                field: name,
                quality: field.quality,
            });
        }
    }
    Ok(())
}

fn normalize_cost(cost: Option<CostDraft>) -> Result<CostInfo, NormalizationError> {
    let Some(cost) = cost else {
        return Ok(CostInfo::not_available());
    };

    let amount = match cost.amount {
        // Re-run the constructor: a draft may have been deserialized straight
        // from JSON, which bypasses `Money::new`'s validation.
        Some(money) => Some(
            Money::new(money.amount_minor, &money.currency, money.minor_unit_exponent)
                .map_err(|error| NormalizationError::InvalidCost(error.to_string()))?,
        ),
        None => None,
    };

    let pricing_version = clean(cost.pricing_version.as_deref());
    match cost.status {
        CostCalculationStatus::ComputedFromPricing if pricing_version.is_none() => {
            return Err(NormalizationError::MissingPricingVersion);
        }
        CostCalculationStatus::NotAvailable if amount.is_some() => {
            return Err(NormalizationError::ContradictoryCostStatus);
        }
        _ => {}
    }

    Ok(CostInfo { amount, status: cost.status, pricing_version })
}

/// Trim a string field, treating blank as absent.
fn clean(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{FieldQuality, SourceProvenance, TokenField, TimestampInterpretation};

    fn provenance() -> SourceProvenance {
        SourceProvenance {
            adapter_id: "claude_code".to_string(),
            adapter_version: "0.1.0".to_string(),
            source_ref: None,
        }
    }

    fn draft() -> UsageRecordDraft {
        UsageRecordDraft::new(SourceApp::ClaudeCode, provenance())
            .with_raw_timestamp("2026-07-29T10:00:00Z")
            .with_tokens(TokenCounts {
                input: TokenField::exact(100),
                output: TokenField::exact(20),
                ..TokenCounts::default()
            })
    }

    fn import_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-29T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn normalization_is_reproducible() {
        let first = normalize_at(draft(), import_time()).unwrap();
        let second = normalize_at(draft(), import_time()).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.normalization_version, NORMALIZATION_VERSION);
        assert_eq!(first.dedupe_algorithm_version, DEDUPE_ALGORITHM_VERSION);
    }

    #[test]
    fn blank_strings_become_unknown() {
        let mut draft = draft();
        draft.model = Some("   ".to_string());
        draft.project = Some(String::new());
        let record = normalize_at(draft, import_time()).unwrap();
        assert_eq!(record.model, None);
        assert_eq!(record.project, None);
    }

    #[test]
    fn missing_counts_do_not_become_zero() {
        let record = normalize_at(draft(), import_time()).unwrap();
        assert_eq!(record.tokens.cached_input.value, None);
        assert_eq!(record.tokens.cached_input.quality, FieldQuality::Unknown);
    }

    #[test]
    fn display_total_sums_input_and_output() {
        let record = normalize_at(draft(), import_time()).unwrap();
        assert_eq!(
            record.display_total,
            Some(DisplayTotal { tokens: 120, rule: TotalRule::InputPlusOutput })
        );
    }

    #[test]
    fn display_total_includes_reasoning_when_reported() {
        let mut draft = draft();
        draft.tokens.reasoning = TokenField::exact(5);
        let record = normalize_at(draft, import_time()).unwrap();
        assert_eq!(
            record.display_total,
            Some(DisplayTotal { tokens: 125, rule: TotalRule::InputPlusOutputPlusReasoning })
        );
    }

    #[test]
    fn reported_total_wins_over_local_sum() {
        let mut draft = draft();
        draft.reported_total_tokens = Some(999);
        let record = normalize_at(draft, import_time()).unwrap();
        assert_eq!(
            record.display_total,
            Some(DisplayTotal { tokens: 999, rule: TotalRule::ReportedBySource })
        );
    }

    #[test]
    fn no_countable_tokens_yields_no_display_total() {
        let draft = UsageRecordDraft::new(SourceApp::Codex, provenance());
        let record = normalize_at(draft, import_time()).unwrap();
        assert_eq!(record.display_total, None);
        assert_eq!(record.timestamp_interpretation, TimestampInterpretation::Missing);
    }

    #[test]
    fn rejects_a_value_free_field_marked_exact() {
        let mut draft = draft();
        draft.tokens.reasoning = TokenField { value: None, quality: FieldQuality::Exact };
        assert!(matches!(
            normalize_at(draft, import_time()),
            Err(NormalizationError::InconsistentTokenQuality { field: "reasoning", .. })
        ));
    }

    #[test]
    fn rejects_computed_cost_without_a_pricing_version() {
        let draft = draft().with_cost(CostDraft {
            amount: Some(Money::new(125, "USD", 2).unwrap()),
            status: CostCalculationStatus::ComputedFromPricing,
            pricing_version: None,
        });
        assert_eq!(normalize_at(draft, import_time()), Err(NormalizationError::MissingPricingVersion));
    }

    #[test]
    fn rejects_an_amount_marked_unavailable() {
        let draft = draft().with_cost(CostDraft {
            amount: Some(Money::new(1, "USD", 2).unwrap()),
            status: CostCalculationStatus::NotAvailable,
            pricing_version: None,
        });
        assert_eq!(
            normalize_at(draft, import_time()),
            Err(NormalizationError::ContradictoryCostStatus)
        );
    }

    #[test]
    fn a_bad_draft_does_not_stop_the_batch() {
        let mut bad = draft();
        bad.tokens.input = TokenField { value: None, quality: FieldQuality::Estimated };
        let batch = normalize_batch_at(vec![draft(), bad, draft()], import_time());
        assert_eq!(batch.records.len(), 2);
        assert_eq!(batch.rejected.len(), 1);
        assert_eq!(batch.rejected[0].index, 1);
        assert_eq!(batch.rejected[0].source_app, SourceApp::ClaudeCode);
    }
}
