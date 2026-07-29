//! The adapter output format.
//!
//! A draft is whatever an adapter could read from its source. It carries no
//! identifier, deduplication key, or import timestamp — the normalizer assigns
//! those — and it is never handed to the repository or to React directly.

use serde::{Deserialize, Serialize};

use super::money::Money;
use super::quality::CostCalculationStatus;
use super::record::SourceProvenance;
use super::source::SourceApp;
use super::tokens::TokenCounts;

/// Cost as an adapter found it, before validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostDraft {
    pub amount: Option<Money>,
    pub status: CostCalculationStatus,
    pub pricing_version: Option<String>,
}

/// One parsed source event, not yet canonical.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecordDraft {
    pub source_app: SourceApp,
    /// The timestamp string exactly as the source wrote it, if it wrote one.
    pub raw_timestamp: Option<String>,
    pub source_event_id: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub tokens: TokenCounts,
    pub reported_total_tokens: Option<u64>,
    pub project: Option<String>,
    pub session_id: Option<String>,
    pub cost: Option<CostDraft>,
    pub provenance: SourceProvenance,
}

impl UsageRecordDraft {
    /// A draft with everything unknown except its source and provenance.
    /// Adapters fill in what their source actually provides.
    pub fn new(source_app: SourceApp, provenance: SourceProvenance) -> Self {
        Self {
            source_app,
            raw_timestamp: None,
            source_event_id: None,
            provider: None,
            model: None,
            tokens: TokenCounts::default(),
            reported_total_tokens: None,
            project: None,
            session_id: None,
            cost: None,
            provenance,
        }
    }

    pub fn with_raw_timestamp(mut self, raw_timestamp: impl Into<String>) -> Self {
        self.raw_timestamp = Some(raw_timestamp.into());
        self
    }

    pub fn with_source_event_id(mut self, id: impl Into<String>) -> Self {
        self.source_event_id = Some(id.into());
        self
    }

    pub fn with_model(mut self, provider: Option<&str>, model: impl Into<String>) -> Self {
        self.provider = provider.map(str::to_string);
        self.model = Some(model.into());
        self
    }

    pub fn with_tokens(mut self, tokens: TokenCounts) -> Self {
        self.tokens = tokens;
        self
    }

    pub fn with_session(mut self, project: Option<&str>, session_id: Option<&str>) -> Self {
        self.project = project.map(str::to_string);
        self.session_id = session_id.map(str::to_string);
        self
    }

    pub fn with_cost(mut self, cost: CostDraft) -> Self {
        self.cost = Some(cost);
        self
    }
}
