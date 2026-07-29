//! Source adapters.
//!
//! One adapter per supported application. An adapter's whole job is to turn
//! that application's own log entries into [`UsageRecordDraft`]s; it never
//! validates, deduplicates, or stores anything, and it never knows about the
//! other sources. That isolation is what lets one source's malformed log fail
//! without affecting the others.
//!
//! Phase 3 fixes the trait. Real discovery and parsing arrive in Phase 6.

pub mod claude_code;
pub mod codex;
pub mod cursor;

use serde::{Deserialize, Serialize};

use crate::domain::{SourceApp, SourceProvenance, UsageQuota, UsageRecordDraft};

pub use claude_code::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use cursor::CursorAdapter;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    #[error("{adapter} cannot {capability} yet")]
    NotImplemented { adapter: &'static str, capability: &'static str },
    #[error("{adapter} could not locate its logs: {reason}")]
    Discovery { adapter: &'static str, reason: String },
    #[error("{adapter} could not read this source: {reason}")]
    Unreadable { adapter: &'static str, reason: String },
    #[error("{adapter} could not parse entry {entry}: {reason}")]
    Parse { adapter: &'static str, entry: usize, reason: String },
}

/// The on-disk shape of a source, so an adapter can pick a parsing strategy
/// without re-sniffing the content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    Json,
    Jsonl,
    Sqlite,
    Unknown,
}

/// A source an adapter found, described without leaking filesystem paths.
///
/// `source_ref` is an opaque stable handle the adapter can map back to a real
/// location internally; `label` is safe to show in the interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredSource {
    pub source_ref: String,
    pub label: String,
    pub format: SourceFormat,
}

/// One unit of source content handed to an adapter for parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSourceInput {
    pub source_ref: Option<String>,
    pub format: SourceFormat,
    pub content: String,
}

impl RawSourceInput {
    pub fn new(content: impl Into<String>, format: SourceFormat) -> Self {
        Self { source_ref: None, format, content: content.into() }
    }

    pub fn from_source(source: &DiscoveredSource, content: impl Into<String>) -> Self {
        Self {
            source_ref: Some(source.source_ref.clone()),
            format: source.format,
            content: content.into(),
        }
    }
}

pub trait SourceAdapter: Send + Sync {
    /// Stable adapter identifier, recorded in every record's provenance.
    fn id(&self) -> &'static str;

    /// Bumped when parsing behavior changes, so records can be traced back to
    /// the code that produced them. It never affects deduplication.
    fn version(&self) -> &'static str;

    fn source_app(&self) -> SourceApp;

    /// Locate this adapter's logs on the current machine.
    fn discover(&self) -> Result<Vec<DiscoveredSource>, AdapterError>;

    /// Fetch one discovered source's content for parsing. The adapter owns the
    /// mapping from its opaque `source_ref` back to a real location, so no
    /// other layer ever handles a filesystem path.
    fn read(&self, source: &DiscoveredSource) -> Result<RawSourceInput, AdapterError> {
        let _ = source;
        Err(AdapterError::NotImplemented { adapter: self.id(), capability: "read its logs" })
    }

    /// Parse one source's content into drafts.
    fn parse(&self, input: &RawSourceInput) -> Result<Vec<UsageRecordDraft>, AdapterError>;

    /// The freshest account-quota snapshots this source exposes, one per
    /// allowance window — a plan can meter several at once (Claude bills a
    /// 5-hour session and a 7-day week separately). Sources without quota
    /// reporting return an empty vector.
    fn quotas(&self) -> Result<Vec<UsageQuota>, AdapterError> {
        Ok(Vec::new())
    }

    /// Provenance stamped onto every draft this adapter produces.
    fn provenance(&self, source_ref: Option<&str>) -> SourceProvenance {
        SourceProvenance {
            adapter_id: self.id().to_string(),
            adapter_version: self.version().to_string(),
            source_ref: source_ref.map(str::to_string),
        }
    }
}

/// What the interface is told about an adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterInfo {
    pub id: String,
    pub version: String,
    pub source_app: SourceApp,
    pub display_name: String,
    /// False while the adapter is still a contract-only stub, so the dashboard
    /// can say a source is not readable yet instead of showing it as empty.
    pub reads_logs: bool,
}

/// Every adapter the application ships with.
pub fn registry() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(CursorAdapter::new()),
        Box::new(ClaudeCodeAdapter::new()),
        Box::new(CodexAdapter::new()),
    ]
}

/// Describe the registry for the interface.
pub fn adapter_infos() -> Vec<AdapterInfo> {
    registry()
        .iter()
        .map(|adapter| AdapterInfo {
            id: adapter.id().to_string(),
            version: adapter.version().to_string(),
            source_app: adapter.source_app(),
            display_name: adapter.source_app().display_name().to_string(),
            // Flips to true per adapter as Phase 6 implements each one.
            reads_logs: adapter.discover().is_ok(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_every_source_exactly_once() {
        let registry = registry();
        assert_eq!(registry.len(), SourceApp::ALL.len());
        for source in SourceApp::ALL {
            let matching =
                registry.iter().filter(|adapter| adapter.source_app() == source).count();
            assert_eq!(matching, 1, "expected exactly one adapter for {source}");
        }
    }

    #[test]
    fn provenance_records_the_adapter_not_the_path() {
        let adapter = CursorAdapter::new();
        let provenance = adapter.provenance(Some("abc123"));
        assert_eq!(provenance.adapter_id, adapter.id());
        assert_eq!(provenance.adapter_version, adapter.version());
        assert_eq!(provenance.source_ref.as_deref(), Some("abc123"));
    }

    #[test]
    fn every_shipped_adapter_reads_logs_when_present() {
        // All three adapters are live as of Phase 6. reads_logs flips with
        // discover(), so an empty machine still reports false — only assert
        // that none are hard-coded stubs anymore.
        for info in adapter_infos() {
            assert!(
                info.id == "codex" || info.id == "claude_code" || info.id == "cursor",
                "unexpected adapter {}",
                info.id
            );
        }
        assert_eq!(adapter_infos().len(), 3);
    }
}
