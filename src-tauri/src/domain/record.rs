//! The normalized usage record — the single format every layer above the
//! adapters consumes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::money::Money;
use super::quality::{CostCalculationStatus, TimestampInterpretation};
use super::source::SourceApp;
use super::tokens::TokenCounts;

/// Bumped whenever the normalized shape or its interpretation rules change.
/// Records normalized under an older version stay identifiable after import.
pub const NORMALIZATION_VERSION: u32 = 1;

/// How a display total was arrived at. Stored so the interface never has to
/// guess whether a total is comparable across sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TotalRule {
    /// The source supplied the total directly.
    ReportedBySource,
    /// input + output.
    InputPlusOutput,
    /// input + output + reasoning.
    InputPlusOutputPlusReasoning,
}

/// A computed or reported total, always paired with the rule that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayTotal {
    pub tokens: u64,
    pub rule: TotalRule,
}

/// Cost for one record. Absent `amount` with a `NotAvailable` status is the
/// normal case for local logs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostInfo {
    pub amount: Option<Money>,
    pub status: CostCalculationStatus,
    /// Identifies the pricing table used when `status` is `ComputedFromPricing`.
    pub pricing_version: Option<String>,
}

impl CostInfo {
    pub fn not_available() -> Self {
        Self { amount: None, status: CostCalculationStatus::NotAvailable, pricing_version: None }
    }
}

impl Default for CostInfo {
    fn default() -> Self {
        Self::not_available()
    }
}

/// Non-sensitive provenance.
///
/// Deliberately excludes filesystem paths, prompts, and responses. `source_ref`
/// is an opaque adapter-chosen handle (for example a hash of a log file's
/// identity) used only to tell one origin from another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceProvenance {
    pub adapter_id: String,
    pub adapter_version: String,
    pub source_ref: Option<String>,
}

/// A validated, canonical usage record.
///
/// Construct these through `crate::normalize`, never by hand outside tests:
/// the identifier, deduplication key, and import timestamp are assigned there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    pub normalization_version: u32,

    /// Stable internal identifier, derived from the deduplication key so the
    /// same source event always yields the same identifier.
    pub id: String,

    /// The timestamp exactly as the source wrote it, kept for auditing.
    pub raw_timestamp: Option<String>,
    /// The instant the event occurred, when it could be resolved.
    pub event_timestamp_utc: Option<DateTime<Utc>>,
    pub timestamp_interpretation: TimestampInterpretation,

    pub source_app: SourceApp,
    pub source_event_id: Option<String>,

    pub dedupe_key: String,
    pub dedupe_algorithm_version: u32,

    pub provider: Option<String>,
    pub model: Option<String>,

    pub tokens: TokenCounts,
    /// The total as the source reported it, independent of any local sum.
    pub reported_total_tokens: Option<u64>,
    /// The total this application shows, absent when nothing can be summed.
    pub display_total: Option<DisplayTotal>,

    pub project: Option<String>,
    pub session_id: Option<String>,

    pub cost: CostInfo,

    pub imported_at: DateTime<Utc>,
    pub provenance: SourceProvenance,
}
