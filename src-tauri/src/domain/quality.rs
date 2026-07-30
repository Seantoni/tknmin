//! Confidence markers.
//!
//! Local logs expose partial information, so every uncertain value carries a
//! status alongside it. A missing value is never silently treated as zero.

use serde::{Deserialize, Serialize};

/// How much to trust a single token count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldQuality {
    /// Reported directly by the source.
    Exact,
    /// Derived from a heuristic, such as a character-based estimate.
    Estimated,
    /// The source reported only part of this category.
    Partial,
    /// The source did not report this category at all.
    #[default]
    Unknown,
}

impl FieldQuality {
    /// Whether a value with this quality may be added into a displayed total.
    pub fn is_countable(self) -> bool {
        matches!(
            self,
            FieldQuality::Exact | FieldQuality::Estimated | FieldQuality::Partial
        )
    }
}

/// How the normalized UTC timestamp was obtained from the raw source value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimestampInterpretation {
    /// The source carried an explicit offset or was an unambiguous epoch value.
    ExplicitOffset,
    /// No zone information was present and UTC was assumed.
    AssumedUtc,
    /// No zone information was present and the machine's local zone was assumed.
    AssumedLocal,
    /// A raw value existed but could not be parsed into an instant.
    Unparsable,
    /// The source carried no timestamp at all.
    Missing,
}

impl TimestampInterpretation {
    /// Whether the normalized instant is safe to use for range filtering.
    pub fn is_resolved(self) -> bool {
        !matches!(
            self,
            TimestampInterpretation::Unparsable | TimestampInterpretation::Missing
        )
    }
}

/// Where a cost figure came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostCalculationStatus {
    /// The source itself reported a monetary amount.
    ReportedBySource,
    /// Computed locally from a pricing table; `pricing_version` identifies it.
    ComputedFromPricing,
    /// Cost is not knowable for this record.
    NotAvailable,
}
