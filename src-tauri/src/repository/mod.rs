//! Storage boundary.
//!
//! The trait exposes only the operations the application currently performs.
//! Widening it is a deliberate act, which is what lets the in-memory backend
//! be swapped for SQLite later without touching the layers above.

pub mod memory;
pub mod summarize;

use crate::domain::{RecentQuery, SummaryQuery, UsageRecord, UsageSummary};

pub use memory::InMemoryUsageRepository;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepositoryError {
    /// A thread panicked while holding the store's lock.
    #[error("the usage store is unavailable")]
    Unavailable,
    /// Reserved for backends that can fail for their own reasons, such as
    /// SQLite in a later phase.
    #[error("storage error: {0}")]
    Backend(String),
}

/// What one `insert_batch` call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsertReport {
    pub inserted: usize,
    /// Records whose deduplication key was already stored.
    pub duplicates_skipped: usize,
}

pub trait UsageRepository: Send + Sync {
    /// Insert records, skipping any whose deduplication key is already
    /// present. Idempotent: importing the same log twice is a no-op.
    fn insert_batch(&self, records: Vec<UsageRecord>) -> Result<InsertReport, RepositoryError>;

    fn summary(&self, query: &SummaryQuery) -> Result<UsageSummary, RepositoryError>;

    fn recent(&self, query: &RecentQuery) -> Result<Vec<UsageRecord>, RepositoryError>;

    fn count(&self) -> Result<usize, RepositoryError>;

    /// Remove everything. Used when a re-import must start from a clean state.
    fn clear(&self) -> Result<(), RepositoryError>;
}
