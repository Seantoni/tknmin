//! Shared vocabulary.
//!
//! This module owns the types every other layer speaks in. It depends on no
//! other module in the crate, which keeps the dependency direction one-way:
//! adapters, normalizer, repository, and commands all point here.

pub mod draft;
pub mod money;
pub mod notifications;
pub mod options;
pub mod quality;
pub mod quota;
pub mod record;
pub mod source;
pub mod summary;
pub mod tokens;

pub use draft::{CostDraft, UsageRecordDraft};
pub use money::{Money, MoneyError};
pub use notifications::{
    evaluate_alerts, AlertAction, ThresholdAlert, HANDOFF_PROMPT,
};
pub use options::{AppOptions, OptionsError, SourceThreshold, ThresholdMetric};
pub use quality::{CostCalculationStatus, FieldQuality, TimestampInterpretation};
pub use quota::UsageQuota;
pub use record::{
    CostInfo, DisplayTotal, SourceProvenance, TotalRule, UsageRecord, NORMALIZATION_VERSION,
};
pub use source::SourceApp;
pub use summary::{
    CostTotal, CurrencyTotal, GroupSummary, RecentQuery, SummaryQuery, TokenTotal, UsageSummary,
    UsageTotals,
};
pub use tokens::{TokenCounts, TokenField};
