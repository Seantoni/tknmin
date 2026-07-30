//! Shared vocabulary.
//!
//! This module owns the types every other layer speaks in. It depends on no
//! other module in the crate, which keeps the dependency direction one-way:
//! adapters, normalizer, repository, and commands all point here.

pub mod baseline;
pub mod draft;
pub mod money;
pub mod notifications;
pub mod options;
pub mod pace;
pub mod quality;
pub mod quota;
pub mod record;
pub mod source;
pub mod summary;
pub mod sync;
pub mod tokens;

pub use baseline::{
    build_baseline, pace_vs_baseline, recent_token_rates, PaceVsBaseline, SourceBaseline,
    TokenRate, BASELINE_DAYS, RATE_WINDOW_MINUTES,
};
pub use draft::{CostDraft, UsageRecordDraft};
pub use money::{Money, MoneyError};
pub use notifications::{
    evaluate_alerts, live_alert_keys, AlertAction, ThresholdAlert, HANDOFF_PROMPT,
};
pub use options::{AppOptions, OptionsError, SourceThreshold, ThresholdMetric};
pub use pace::{evaluate_pace, PaceBasis, PaceState, QuotaSample, WindowPace};
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
pub use sync::{
    RepositoryRevision, ReplaceScope, SourceCheckpoint, SourceSyncHealth, SyncState,
};
pub use tokens::{TokenCounts, TokenField};
