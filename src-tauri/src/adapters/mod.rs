//! Source adapters.
//!
//! One adapter per supported application. An adapter's whole job is to turn
//! the delta the coordinator asks for into [`UsageRecordDraft`]s; it never
//! validates, deduplicates, or stores anything, and it never knows about the
//! other sources. That isolation is what lets one source's malformed log fail
//! without affecting the others.
//!
//! What an adapter must never do — these belong to `crate::refresh`, which is
//! the single owner of refresh orchestration:
//!
//! - start a thread, timer, watcher, or retry loop
//! - decide its own cadence, or call itself on focus, startup, or file change
//! - emit an event, touch global state, or refresh the menu bar or alerts
//! - keep a private checkpoint or quota cache competing with the repository
//!
//! An adapter is a function from *(checkpoints, mode)* to *(drafts, new
//! checkpoints)*. It reads files and, for Cursor, the network; everything
//! about *when* is decided elsewhere.

pub mod claude_code;
pub mod codex;
pub mod cursor;

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{
    ReplaceScope, SourceApp, SourceCheckpoint, SourceProvenance, UsageQuota, UsageRecordDraft,
};

pub use claude_code::ClaudeCodeAdapter;
pub use codex::CodexAdapter;
pub use cursor::CursorAdapter;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    #[error("{adapter} cannot {capability} yet")]
    NotImplemented {
        adapter: &'static str,
        capability: &'static str,
    },
    #[error("{adapter} could not locate its logs: {reason}")]
    Discovery {
        adapter: &'static str,
        reason: String,
    },
    #[error("{adapter} could not read this source: {reason}")]
    Unreadable {
        adapter: &'static str,
        reason: String,
    },
    #[error("{adapter} could not parse entry {entry}: {reason}")]
    Parse {
        adapter: &'static str,
        entry: usize,
        reason: String,
    },
    /// The source could not be reached. Distinguished from every other failure
    /// because it is the one the interface should call "offline" rather than
    /// "broken", and the one a network-recovery trigger should retry.
    #[error("{adapter} could not reach its source: {reason}")]
    Offline {
        adapter: &'static str,
        reason: String,
    },
    /// The provider asked to be left alone. `retry_after` is its own answer
    /// when it gave one; the coordinator's backoff decides the rest.
    #[error("{adapter} was rate limited by its source")]
    RateLimited {
        adapter: &'static str,
        retry_after: Option<std::time::Duration>,
    },
}

impl AdapterError {
    /// Whether this failure means "unreachable" rather than "wrong".
    pub fn is_offline(&self) -> bool {
        matches!(
            self,
            AdapterError::Offline { .. } | AdapterError::RateLimited { .. }
        )
    }

    /// How long the source asked to be left alone, when it said so.
    pub fn retry_after(&self) -> Option<std::time::Duration> {
        match self {
            AdapterError::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
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
        Self {
            source_ref: None,
            format,
            content: content.into(),
        }
    }

    pub fn from_source(source: &DiscoveredSource, content: impl Into<String>) -> Self {
        Self {
            source_ref: Some(source.source_ref.clone()),
            format: source.format,
            content: content.into(),
        }
    }
}

/// How much of a source the coordinator wants read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// Only what changed since the supplied checkpoints. The fast path, and
    /// the one every filesystem event takes.
    Incremental,
    /// Re-read a bounded window and reconcile it, so corrections, deletions,
    /// and events the incremental path missed converge. The safety net.
    Reconcile,
    /// Allowance windows only — no log walk. Used by the quota lane, which
    /// runs on its own schedule so a slow account endpoint cannot delay local
    /// token ingestion.
    QuotaOnly,
}

/// What the coordinator asks a source for.
#[derive(Debug, Clone)]
pub struct DeltaRequest {
    pub mode: SyncMode,
    /// Everything this adapter checkpointed, as last committed.
    pub checkpoints: Vec<SourceCheckpoint>,
    pub now: DateTime<Utc>,
}

impl DeltaRequest {
    /// A request with no history, which is what a first run sees.
    pub fn fresh(mode: SyncMode) -> Self {
        Self {
            mode,
            checkpoints: Vec::new(),
            now: Utc::now(),
        }
    }

    pub fn checkpoint(&self, source_key: &str) -> Option<&SourceCheckpoint> {
        self.checkpoints
            .iter()
            .find(|checkpoint| checkpoint.source_key == source_key)
    }

    /// A checkpoint's payload decoded into the adapter's own type. A payload
    /// this build cannot read is treated as absent, which costs one full
    /// re-read and never a wrong resume point.
    pub fn resume<T: serde::de::DeserializeOwned>(&self, source_key: &str) -> Option<T> {
        serde_json::from_value(self.checkpoint(source_key)?.payload.clone()).ok()
    }
}

/// What one source produced for one request.
///
/// Everything here commits together. In particular a checkpoint is only ever
/// advanced in the same transaction as the drafts it accounts for, so a failed
/// write can never make the next run skip data.
#[derive(Debug, Clone, Default)]
pub struct SourceDelta {
    pub drafts: Vec<UsageRecordDraft>,
    pub checkpoints: Vec<SourceCheckpoint>,
    /// Time slices this batch replaces wholesale. Set where the source has no
    /// immutable identifier and a correction can only be applied by swapping a
    /// bounded window.
    pub replace_scopes: Vec<ReplaceScope>,
    pub quotas: Vec<UsageQuota>,
    /// True when `quotas` is a complete, authoritative statement of this
    /// source's live windows, so expired ones can be dropped. Never set by a
    /// partial or failed read.
    pub replace_quotas: bool,
    /// When the *source* says its data was true, which is not when this app
    /// read it. Cursor publishes billing well after the activity it bills.
    pub source_observed_at: Option<DateTime<Utc>>,
    /// Activity was detected but the authoritative numbers have not caught up.
    pub awaiting_upstream: bool,
    /// Per-unit failures that did not stop the rest. Already free of tokens
    /// and absolute paths.
    pub failures: Vec<String>,
}

impl SourceDelta {
    pub fn is_empty(&self) -> bool {
        self.drafts.is_empty()
            && self.checkpoints.is_empty()
            && self.quotas.is_empty()
            && self.replace_scopes.is_empty()
    }
}

/// Static facts about a source, cheap enough to answer without touching disk.
///
/// Deliberately not "does discovery succeed": answering that walks live
/// sources, which is refresh work and belongs to the coordinator. Live status
/// reaches the interface through committed [`crate::domain::SourceSyncHealth`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdapterInfo {
    pub id: String,
    pub version: String,
    pub source_app: SourceApp,
    pub display_name: String,
    /// Whether this source needs the network, so the interface can explain an
    /// offline state instead of calling it an error.
    pub needs_network: bool,
}

/// A directory to watch, and how deeply.
///
/// The depth belongs to the adapter because only it knows the shape of what it
/// is watching. A tree of transcripts must be recursive — a subagent creates a
/// nested directory mid-session and its usage appears nowhere else. A single
/// well-known file must not be, and saying so matters: the only way to see an
/// atomic replacement of `~/.claude.json` is to watch the directory holding
/// it, which is the user's home. Recursively, that is a subscription to every
/// write anywhere under `$HOME` — every `npm install`, every download, every
/// `~/Library` churn — each one waking the classifier for a file no source has
/// ever heard of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchRoot {
    pub path: PathBuf,
    /// True for a tree whose interesting files can appear at any depth.
    pub recursive: bool,
}

impl WatchRoot {
    /// A tree: everything under `path`, however deep.
    pub fn tree(path: PathBuf) -> Self {
        Self {
            path,
            recursive: true,
        }
    }

    /// One directory's own entries, and nothing below them.
    pub fn shallow(path: PathBuf) -> Self {
        Self {
            path,
            recursive: false,
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

    /// Directories whose contents mean this source may have changed.
    ///
    /// The adapter only names them. Registering, debouncing, and translating
    /// callbacks into work is the coordinator's job — an adapter that watched
    /// its own files would be a second refresh owner.
    fn watch_roots(&self) -> Vec<WatchRoot> {
        Vec::new()
    }

    /// Whether reading this source can require the network, which decides
    /// whether it gets its own lane. A slow Cursor request must never delay
    /// Codex or Claude Code.
    fn needs_network(&self) -> bool {
        false
    }

    /// Read exactly what was asked for, and nothing more.
    fn read_delta(&self, request: &DeltaRequest) -> Result<SourceDelta, AdapterError>;

    /// Provenance stamped onto every draft this adapter produces.
    fn provenance(&self, source_ref: Option<&str>) -> SourceProvenance {
        SourceProvenance {
            adapter_id: self.id().to_string(),
            adapter_version: self.version().to_string(),
            source_ref: source_ref.map(str::to_string),
        }
    }
}

/// Every adapter the application ships with.
pub fn registry() -> Vec<Box<dyn SourceAdapter>> {
    vec![
        Box::new(CursorAdapter::new()),
        Box::new(ClaudeCodeAdapter::new()),
        Box::new(CodexAdapter::new()),
    ]
}

/// Describe the registry for the interface. Reads nothing.
pub fn adapter_infos() -> Vec<AdapterInfo> {
    registry()
        .iter()
        .map(|adapter| AdapterInfo {
            id: adapter.id().to_string(),
            version: adapter.version().to_string(),
            source_app: adapter.source_app(),
            display_name: adapter.source_app().display_name().to_string(),
            needs_network: adapter.needs_network(),
        })
        .collect()
}

/// Read a file's bytes from `offset` on, tolerating truncation.
///
/// Returns the decoded text, the offset it ends at, and whether the file had
/// to be restarted. A file that shrank was replaced or rotated: the bytes at
/// the old offset belong to different content now, so resuming there would
/// splice two files together. Restarting is slower exactly once, and never
/// wrong.
pub(crate) fn read_from_offset(
    path: &std::path::Path,
    offset: u64,
) -> Result<(String, u64, bool), std::io::Error> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let length = file.metadata()?.len();
    let restarted = length < offset;
    let start = if restarted { 0 } else { offset };

    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(length.saturating_sub(start) as usize);
    file.read_to_end(&mut bytes)?;
    let read_to = start + bytes.len() as u64;

    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        read_to,
        restarted,
    ))
}

/// Split appended text into complete lines and the incomplete tail.
///
/// A source appending JSONL is routinely caught mid-line. Parsing that tail
/// would produce a malformed record or drop a real one, so it is carried to
/// the next attempt instead — the checkpoint stores it verbatim.
pub(crate) fn split_complete_lines(text: &str) -> (&str, &str) {
    match text.rfind('\n') {
        Some(index) => (&text[..=index], &text[index + 1..]),
        None => ("", text),
    }
}

/// A file identity that survives a rename.
///
/// Codex moves a rollout into `archived_sessions/` when a session ends. Keyed
/// by path, that move looks like a brand new file and every event in it would
/// be counted a second time; keyed by device and inode, it is the same rollout
/// in a new place.
#[cfg(unix)]
pub(crate) fn file_identity(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some(format!("{}:{}", metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
pub(crate) fn file_identity(path: &std::path::Path) -> Option<String> {
    Some(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_covers_every_source_exactly_once() {
        let registry = registry();
        assert_eq!(registry.len(), SourceApp::ALL.len());
        for source in SourceApp::ALL {
            let matching = registry
                .iter()
                .filter(|adapter| adapter.source_app() == source)
                .count();
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
    fn describing_the_registry_touches_no_source() {
        let infos = adapter_infos();
        assert_eq!(infos.len(), 3);
        let cursor = infos.iter().find(|info| info.id == "cursor").unwrap();
        assert!(cursor.needs_network);
        assert!(infos
            .iter()
            .filter(|info| info.id != "cursor")
            .all(|info| !info.needs_network));
    }

    #[test]
    fn an_incomplete_final_line_is_held_back() {
        let (complete, partial) = split_complete_lines("{\"a\":1}\n{\"b\":2}\n{\"c\"");
        assert_eq!(complete, "{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(partial, "{\"c\"");

        // Nothing complete yet: the whole chunk waits.
        let (complete, partial) = split_complete_lines("{\"a\"");
        assert_eq!(complete, "");
        assert_eq!(partial, "{\"a\"");
    }

    #[test]
    fn reading_past_the_end_of_a_truncated_file_restarts_it() {
        let directory = std::env::temp_dir().join("tokens-offset-test");
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("log.jsonl");
        std::fs::write(&path, "short").unwrap();

        let (text, read_to, restarted) = read_from_offset(&path, 4_096).unwrap();
        assert!(restarted);
        assert_eq!(text, "short");
        assert_eq!(read_to, 5);

        let (text, read_to, restarted) = read_from_offset(&path, 2).unwrap();
        assert!(!restarted);
        assert_eq!(text, "ort");
        assert_eq!(read_to, 5);

        let _ = std::fs::remove_dir_all(&directory);
    }
}
