//! Aggregates the dashboard reads.
//!
//! Totals never hide missing data: each one reports how many records it could
//! count and how many it could not.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::money::Money;
use super::source::SourceApp;

/// A summed token category.
///
/// `tokens` is the sum over records that actually reported the category.
/// `unknown_records` is how many records did not, so the interface can say
/// "at least N" rather than implying the sum is complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenTotal {
    pub tokens: u64,
    pub counted_records: usize,
    pub unknown_records: usize,
}

impl TokenTotal {
    pub fn add(&mut self, value: Option<u64>) {
        match value {
            Some(value) => {
                self.tokens = self.tokens.saturating_add(value);
                self.counted_records += 1;
            }
            None => self.unknown_records += 1,
        }
    }

    /// True when at least one record in the group lacked this category.
    pub fn is_partial(&self) -> bool {
        self.unknown_records > 0
    }
}

/// Cost accumulated within one currency. Currencies are never mixed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CurrencyTotal {
    pub amount: Money,
    pub counted_records: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostTotal {
    pub by_currency: Vec<CurrencyTotal>,
    pub records_without_cost: usize,
}

/// Every total reported for a set of records.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub record_count: usize,
    pub input: TokenTotal,
    pub output: TokenTotal,
    pub cached_input: TokenTotal,
    pub reasoning: TokenTotal,
    /// Sum of each record's display total, which may follow different rules
    /// per record; `TokenTotal::unknown_records` counts records with none.
    pub display_total: TokenTotal,
    pub cost: CostTotal,
}

/// How a breakdown row is identified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupSummary {
    /// Stable key: the source identifier, the model name, or `null` for the
    /// group of records where the dimension was unknown.
    pub key: Option<String>,
    pub label: String,
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    pub generated_at: DateTime<Utc>,
    pub totals: UsageTotals,
    pub by_source: Vec<GroupSummary>,
    pub by_model: Vec<GroupSummary>,
    /// Records skipped because a date filter was applied and their timestamp
    /// could not be resolved. Zero when no date filter is active.
    pub undated_records_excluded: usize,
}

/// Filters the dashboard may apply. Every field is optional; an empty query
/// summarizes everything.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SummaryQuery {
    pub sources: Option<Vec<SourceApp>>,
    pub models: Option<Vec<String>>,
    /// Inclusive lower bound on the normalized event timestamp.
    pub from: Option<DateTime<Utc>>,
    /// Exclusive upper bound on the normalized event timestamp.
    pub until: Option<DateTime<Utc>>,
}

impl SummaryQuery {
    pub fn has_date_bounds(&self) -> bool {
        self.from.is_some() || self.until.is_some()
    }
}

/// Parameters for the recent-records list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RecentQuery {
    pub limit: usize,
    pub filter: SummaryQuery,
}

impl RecentQuery {
    pub const DEFAULT_LIMIT: usize = 50;
    pub const MAX_LIMIT: usize = 500;

    /// Clamps the caller's limit into the supported range so a bad value from
    /// the interface cannot ask for an unbounded result.
    pub fn effective_limit(&self) -> usize {
        self.limit.clamp(1, Self::MAX_LIMIT)
    }
}

impl Default for RecentQuery {
    fn default() -> Self {
        Self { limit: Self::DEFAULT_LIMIT, filter: SummaryQuery::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_total_separates_missing_from_zero() {
        let mut total = TokenTotal::default();
        total.add(Some(10));
        total.add(None);
        total.add(Some(0));
        assert_eq!(total.tokens, 10);
        assert_eq!(total.counted_records, 2);
        assert_eq!(total.unknown_records, 1);
        assert!(total.is_partial());
    }

    #[test]
    fn recent_query_clamps_limit() {
        assert_eq!(RecentQuery { limit: 0, ..Default::default() }.effective_limit(), 1);
        assert_eq!(RecentQuery { limit: 10_000, ..Default::default() }.effective_limit(), RecentQuery::MAX_LIMIT);
    }
}
