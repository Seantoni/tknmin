//! Codex adapter.
//!
//! Reads Codex's session rollouts: one JSONL file per session under
//! `~/.codex/sessions/YYYY/MM/DD/`, plus archived copies under
//! `~/.codex/archived_sessions/`. Usage arrives in `token_count` events whose
//! `last_token_usage` is one API call's worth of tokens; each becomes a draft.
//! The model comes from the enclosing `turn_context`, the provider from
//! `session_meta`. Codex reports no cost.
//!
//! Rollout files are append-only, which is what makes tailing them safe: each
//! run reads only the bytes past its committed offset. Everything a later
//! chunk needs to make sense of itself — the rollout id, the current model,
//! the working directory, the next event ordinal — is carried in the
//! checkpoint, because the `session_meta` line that established it may be
//! megabytes behind.
//!
//! The n-th `token_count` event in a rollout is a stable identity:
//! `{rollout_id}:{n}`. That is what makes the sessions/archived overlap
//! harmless — a rollout moved into the archive keeps its inode, so it resumes
//! where it left off instead of being counted twice.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::domain::{
    SourceApp, SourceCheckpoint, TokenCounts, TokenField, UsageQuota, UsageRecordDraft,
};

use super::{
    file_identity, read_from_offset, split_complete_lines, AdapterError, DeltaRequest,
    DiscoveredSource, RawSourceInput, SourceAdapter, SourceDelta, SyncMode,
};

const SESSIONS_DIR: &str = "sessions";
const ARCHIVED_DIR: &str = "archived_sessions";
const ROLLOUT_SUFFIX: &str = ".jsonl";

/// Everything the next chunk of a rollout needs to interpret itself.
///
/// Persisted verbatim by the repository, so it carries no path and no
/// credential: the file is found again by walking the tree and matching
/// identity.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RolloutCheckpoint {
    /// Bytes already accounted for. Reading resumes here.
    pub offset: u64,
    /// The trailing bytes of an incomplete line, held for the next attempt.
    #[serde(default)]
    pub partial: String,
    /// The next `token_count` ordinal, which is what makes event identity
    /// stable without re-reading the file from the start.
    #[serde(default)]
    pub next_ordinal: usize,
    #[serde(default)]
    pub rollout_id: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Last size seen, so an unchanged file can be skipped without opening it.
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct CodexAdapter {
    /// The `.codex` directory. Overridable so tests never touch real logs.
    root: PathBuf,
}

impl CodexAdapter {
    pub const ID: &'static str = "codex";
    pub const VERSION: &'static str = "0.3.0";

    pub fn new() -> Self {
        Self {
            root: default_codex_dir(),
        }
    }

    pub fn with_root(root: PathBuf) -> Self {
        Self { root }
    }

    fn rollout_files(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect_rollouts(&self.root.join(SESSIONS_DIR), true, &mut files);
        collect_rollouts(&self.root.join(ARCHIVED_DIR), false, &mut files);
        files.sort();
        files
    }
}

impl Default for CodexAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceAdapter for CodexAdapter {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn version(&self) -> &'static str {
        Self::VERSION
    }

    fn source_app(&self) -> SourceApp {
        SourceApp::Codex
    }

    /// Both trees, because a rollout is written under `sessions/` and moved
    /// under `archived_sessions/` when the session ends.
    fn watch_roots(&self) -> Vec<PathBuf> {
        vec![
            self.root.join(SESSIONS_DIR),
            self.root.join(ARCHIVED_DIR),
        ]
    }

    fn read_delta(&self, request: &DeltaRequest) -> Result<SourceDelta, AdapterError> {
        if request.mode == SyncMode::QuotaOnly {
            return Ok(SourceDelta {
                quotas: self.quotas()?,
                replace_quotas: true,
                ..SourceDelta::default()
            });
        }

        let files = self.rollout_files();
        if files.is_empty() {
            return Err(AdapterError::Discovery {
                adapter: Self::ID,
                reason: "no rollout logs found".to_string(),
            });
        }

        let mut delta = SourceDelta::default();
        let mut freshest_quota: Option<UsageQuota> = None;

        for path in files {
            let Some(source_key) = file_identity(&path) else {
                continue;
            };
            let stored: RolloutCheckpoint = request.resume(&source_key).unwrap_or_default();

            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let size = metadata.len();

            // The cheap gate, and the whole reason reconciliation is
            // affordable: a rollout whose size matches the last committed one
            // has nothing new, and Codex never rewrites what it already wrote.
            // A missed watcher event shows up here as a size that moved, so
            // the periodic pass repairs it without re-reading the history.
            if size == stored.size && stored.offset == size && stored.partial.is_empty() {
                continue;
            }

            let (appended, read_to, restarted) = match read_from_offset(&path, stored.offset) {
                Ok(result) => result,
                Err(error) => {
                    delta
                        .failures
                        .push(format!("a Codex rollout could not be read: {error}"));
                    continue;
                }
            };

            // A restarted file lost its resume point; its ordinals have to be
            // recomputed from zero or identities would collide with the old
            // ones. Everything else about the session is re-read too.
            let mut cursor = if restarted {
                RolloutCheckpoint::default()
            } else {
                stored.clone()
            };

            let carried = std::mem::take(&mut cursor.partial);
            let text = format!("{carried}{appended}");
            let (complete, partial) = split_complete_lines(&text);

            // Provenance names the rollout, not the file: the same session
            // read from `sessions/` and from `archived_sessions/` must look
            // like one origin, and the id in the filename is what says so.
            let source_ref = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .and_then(rollout_id_from_stem)
                .map(str::to_string)
                .unwrap_or_else(|| source_key.clone());

            let parsed = parse_chunk(self, &source_ref, complete, &mut cursor);
            delta.drafts.extend(parsed.drafts);
            if let Some(quota) = parsed.quota {
                if freshest_quota
                    .as_ref()
                    .is_none_or(|kept| quota.observed_at > kept.observed_at)
                {
                    freshest_quota = Some(quota);
                }
            }

            cursor.partial = partial.to_string();
            cursor.offset = read_to;
            cursor.size = size;

            delta.checkpoints.push(SourceCheckpoint {
                adapter_id: Self::ID.to_string(),
                source_key,
                payload: serde_json::to_value(&cursor).unwrap_or_default(),
            });
        }

        // Codex stamps its allowance onto every token event, so the appended
        // bytes already answered the quota question. Falling back to the
        // wider scan only when nothing was appended keeps the fast path free.
        delta.source_observed_at = freshest_quota.as_ref().map(|quota| quota.observed_at);
        match freshest_quota {
            Some(quota) => {
                delta.quotas = vec![quota];
                delta.replace_quotas = true;
            }
            None if request.mode == SyncMode::Reconcile => {
                delta.quotas = self.quotas().unwrap_or_default();
                delta.replace_quotas = !delta.quotas.is_empty();
            }
            None => {}
        }

        Ok(delta)
    }
}

impl CodexAdapter {
    /// Locate this adapter's rollouts. Kept for tests and diagnostics; the
    /// running application reaches sources through [`SourceAdapter::read_delta`].
    pub fn discover(&self) -> Result<Vec<DiscoveredSource>, AdapterError> {
        let files = self.rollout_files();
        if files.is_empty() {
            return Err(AdapterError::Discovery {
                adapter: Self::ID,
                reason: format!("no rollout logs found under {}", self.root.display()),
            });
        }

        Ok(files
            .into_iter()
            .filter_map(|path| {
                let stem = path.file_stem()?.to_str()?.to_string();
                let source_ref = rollout_id_from_stem(&stem)?.to_string();
                Some(DiscoveredSource {
                    source_ref,
                    label: stem,
                    format: super::SourceFormat::Jsonl,
                })
            })
            .collect())
    }

    pub fn read(&self, source: &DiscoveredSource) -> Result<RawSourceInput, AdapterError> {
        let path = self
            .rollout_files()
            .into_iter()
            .find(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .and_then(rollout_id_from_stem)
                    == Some(source.source_ref.as_str())
            })
            .ok_or_else(|| AdapterError::Unreadable {
                adapter: Self::ID,
                reason: "the rollout file is no longer where discovery found it".to_string(),
            })?;

        let content = fs::read_to_string(&path).map_err(|error| AdapterError::Unreadable {
            adapter: Self::ID,
            reason: format!("{}: {error}", path.display()),
        })?;

        Ok(RawSourceInput::from_source(source, content))
    }

    /// Parse a whole rollout from the start. Used by tests and by the
    /// fixture path; the running application parses chunks instead.
    pub fn parse(&self, input: &RawSourceInput) -> Result<Vec<UsageRecordDraft>, AdapterError> {
        let mut cursor = RolloutCheckpoint::default();
        let source_ref = input.source_ref.clone().unwrap_or_else(|| "unknown".to_string());
        Ok(parse_chunk(self, &source_ref, &input.content, &mut cursor).drafts)
    }

    /// Codex meters a single weekly window, so this is at most one snapshot.
    pub fn quotas(&self) -> Result<Vec<UsageQuota>, AdapterError> {
        // Session files are named for when a session *started*, so a stale
        // path can still hold fresh events. Modification time is the honest
        // recency signal; only the few freshest files are worth opening.
        const CANDIDATE_FILES: usize = 10;

        let mut files = self.rollout_files();
        files.sort_by_key(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok());
        let candidates: Vec<_> = files.into_iter().rev().take(CANDIDATE_FILES).collect();

        let mut freshest: Option<UsageQuota> = None;
        for path in candidates {
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let Some(quota) = latest_quota_in(&content) else {
                continue;
            };
            if freshest
                .as_ref()
                .is_none_or(|current| quota.observed_at > current.observed_at)
            {
                freshest = Some(quota);
            }
        }
        Ok(freshest.into_iter().collect())
    }
}

/// What one chunk of a rollout yielded.
struct ParsedChunk {
    drafts: Vec<UsageRecordDraft>,
    /// The freshest allowance snapshot in the chunk. Codex stamps
    /// `rate_limits` onto every token event, so appended bytes answer the
    /// quota question for free.
    quota: Option<UsageQuota>,
}

/// Parse complete lines, advancing the cursor as it goes.
///
/// The cursor is both input and output: a chunk starting mid-file inherits the
/// model, provider, and ordinal established by earlier chunks, and leaves them
/// updated for the next one. That is what lets a 200 MB rollout cost one read
/// of the bytes that actually arrived.
fn parse_chunk(
    adapter: &CodexAdapter,
    source_ref: &str,
    complete_lines: &str,
    cursor: &mut RolloutCheckpoint,
) -> ParsedChunk {
    let mut drafts = Vec::new();
    let mut quota = None;

    for line in complete_lines.lines() {
        // Rollouts are mostly conversation content; only these three line
        // shapes carry anything an import needs, so the rest is skipped
        // before the JSON parser ever sees it.
        let interesting = line.contains(r#""type":"session_meta""#)
            || line.contains(r#""type":"turn_context""#)
            || line.contains(r#""type":"token_count""#);
        if !interesting {
            continue;
        }

        let parsed: RolloutLine = match serde_json::from_str(line) {
            Ok(parsed) => parsed,
            // A line this build cannot make sense of is skipped rather than
            // failing the file around it.
            Err(_) => continue,
        };
        let Some(payload) = parsed.payload else {
            continue;
        };

        match parsed.kind.as_str() {
            "session_meta" => {
                if let Ok(meta) = serde_json::from_value::<SessionMeta>(payload) {
                    apply_meta(cursor, meta);
                }
            }
            "turn_context" => {
                if let Ok(turn) = serde_json::from_value::<TurnContext>(payload) {
                    apply_turn(cursor, turn);
                }
            }
            "event_msg" => {
                if let Some(snapshot) = quota_from_payload(&payload, parsed.timestamp.as_deref()) {
                    if quota
                        .as_ref()
                        .is_none_or(|kept: &UsageQuota| snapshot.observed_at > kept.observed_at)
                    {
                        quota = Some(snapshot);
                    }
                }

                let Ok(EventPayload { kind, info }) =
                    serde_json::from_value::<EventPayload>(payload)
                else {
                    continue;
                };
                if kind != "token_count" {
                    continue;
                }
                let Some(usage) = info.and_then(|info| info.last_token_usage) else {
                    continue;
                };

                let rollout_id = cursor.rollout_id.clone().unwrap_or_else(|| source_ref.to_string());
                let ordinal = cursor.next_ordinal;
                cursor.next_ordinal += 1;

                let mut draft =
                    UsageRecordDraft::new(SourceApp::Codex, adapter.provenance(Some(source_ref)))
                        .with_raw_timestamp(parsed.timestamp.unwrap_or_default())
                        .with_source_event_id(format!("{rollout_id}:{ordinal}"))
                        .with_tokens(TokenCounts {
                            input: usage
                                .input_tokens
                                .map_or_else(TokenField::unknown, TokenField::exact),
                            output: usage
                                .output_tokens
                                .map_or_else(TokenField::unknown, TokenField::exact),
                            cached_input: usage
                                .cached_input_tokens
                                .map_or_else(TokenField::unknown, TokenField::exact),
                            reasoning: usage
                                .reasoning_output_tokens
                                .map_or_else(TokenField::unknown, TokenField::exact),
                        })
                        .with_session(project_of(cursor).as_deref(), Some(&rollout_id));
                draft.provider = cursor.provider.clone();
                draft.model = cursor.model.clone();
                draft.reported_total_tokens = usage.total_tokens;
                drafts.push(draft);
            }
            _ => {}
        }
    }

    ParsedChunk { drafts, quota }
}

fn apply_meta(cursor: &mut RolloutCheckpoint, meta: SessionMeta) {
    if meta.id.is_some() {
        cursor.rollout_id = meta.id;
    }
    if meta.model_provider.is_some() {
        cursor.provider = meta.model_provider;
    }
    if cursor.cwd.is_none() {
        cursor.cwd = meta.cwd;
    }
}

fn apply_turn(cursor: &mut RolloutCheckpoint, turn: TurnContext) {
    // A session can switch models between turns; the latest context wins, and
    // it has to survive into the next chunk or every later event would be
    // attributed to the model the session opened with.
    if turn.model.is_some() {
        cursor.model = turn.model;
    }
    if turn.cwd.is_some() {
        cursor.cwd = turn.cwd;
    }
}

/// The project is the working directory's final component — the only part of
/// the path a record is allowed to carry.
fn project_of(cursor: &RolloutCheckpoint) -> Option<String> {
    cursor
        .cwd
        .as_deref()?
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

/// The allowance window one token event carries, if it carries one.
fn quota_from_payload(payload: &serde_json::Value, timestamp: Option<&str>) -> Option<UsageQuota> {
    let window = serde_json::from_value::<EventQuota>(payload.clone())
        .ok()?
        .rate_limits?
        .primary?;
    let used_percent_tenths = (window.used_percent * 10.0).round();
    if !(0.0..=10_000.0).contains(&used_percent_tenths) {
        return None;
    }
    Some(UsageQuota {
        source_app: SourceApp::Codex,
        label: None,
        window_minutes: window.window_minutes,
        used_percent_tenths: used_percent_tenths as u16,
        resets_at: Some(chrono::DateTime::from_timestamp(window.resets_at, 0)?),
        observed_at: chrono::DateTime::parse_from_rfc3339(timestamp?)
            .ok()?
            .to_utc(),
    })
}

/// The last quota snapshot a rollout recorded, if any. Codex stamps
/// `rate_limits` onto every `token_count` event, so the freshest one in the
/// file is the answer; only the weekly `primary` window is surfaced today
/// (`secondary` has been null in every observed file).
fn latest_quota_in(content: &str) -> Option<UsageQuota> {
    for line in content.lines().rev() {
        if !line.contains(r#""type":"token_count""#) {
            continue;
        }
        let line: RolloutLine = match serde_json::from_str(line) {
            Ok(line) => line,
            Err(_) => continue,
        };
        let window = line
            .payload
            .and_then(|payload| serde_json::from_value::<EventQuota>(payload).ok())
            .and_then(|event| event.rate_limits)
            .and_then(|limits| limits.primary);
        let Some(window) = window else { continue };

        let used_percent_tenths = (window.used_percent * 10.0).round();
        if !(0.0..=10_000.0).contains(&used_percent_tenths) {
            continue;
        }
        return Some(UsageQuota {
            source_app: SourceApp::Codex,
            label: None,
            window_minutes: window.window_minutes,
            used_percent_tenths: used_percent_tenths as u16,
            resets_at: Some(chrono::DateTime::from_timestamp(window.resets_at, 0)?),
            observed_at: chrono::DateTime::parse_from_rfc3339(&line.timestamp?)
                .ok()?
                .to_utc(),
        });
    }
    None
}

#[derive(Deserialize)]
struct EventQuota {
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct RateLimits {
    primary: Option<RateWindow>,
}

#[derive(Deserialize)]
struct RateWindow {
    used_percent: f64,
    window_minutes: u32,
    resets_at: i64,
}

#[derive(Deserialize)]
struct RolloutLine {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    payload: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct SessionMeta {
    id: Option<String>,
    cwd: Option<String>,
    model_provider: Option<String>,
}

#[derive(Deserialize)]
struct TurnContext {
    model: Option<String>,
    cwd: Option<String>,
}

#[derive(Deserialize)]
struct EventPayload {
    #[serde(rename = "type")]
    kind: String,
    info: Option<TokenCountInfo>,
}

#[derive(Deserialize)]
struct TokenCountInfo {
    last_token_usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct TokenUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    reasoning_output_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

/// `rollout-2026-07-28T14-04-50-<uuid>` → `<uuid>`. The id is the record-safe
/// handle for a file: stable, and free of filesystem paths. Both the timestamp
/// and the UUID contain dashes, so split off the fixed-width UUID at the end.
fn rollout_id_from_stem(stem: &str) -> Option<&str> {
    const UUID_LEN: usize = 36;
    let stem = stem.strip_prefix("rollout-")?;
    if stem.len() > UUID_LEN && stem.as_bytes()[stem.len() - UUID_LEN - 1] == b'-' {
        Some(&stem[stem.len() - UUID_LEN..])
    } else {
        None
    }
}

fn collect_rollouts(dir: &Path, recurse: bool, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if recurse {
                collect_rollouts(&path, recurse, out);
            }
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(ROLLOUT_SUFFIX))
        {
            out.push(path);
        }
    }
}

fn default_codex_dir() -> PathBuf {
    std::env::var_os("HOME").map_or_else(
        || PathBuf::from("."),
        |home| PathBuf::from(home).join(".codex"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::SourceFormat;

    const SAMPLE: &str = include_str!("../../fixtures/samples/codex-rollout.jsonl");

    fn parse_sample() -> Vec<UsageRecordDraft> {
        let adapter = CodexAdapter::new();
        let input = RawSourceInput {
            source_ref: Some("019faa1d-8971-72a3-9a72-1eeb61a00ef4".to_string()),
            format: SourceFormat::Jsonl,
            content: SAMPLE.to_string(),
        };
        adapter.parse(&input).unwrap()
    }

    #[test]
    fn parses_one_draft_per_token_count_event() {
        let drafts = parse_sample();
        assert_eq!(drafts.len(), 5);

        let first = &drafts[0];
        assert_eq!(first.source_app, SourceApp::Codex);
        assert_eq!(
            first.raw_timestamp.as_deref(),
            Some("2026-07-28T19:04:56.984Z")
        );
        assert_eq!(
            first.source_event_id.as_deref(),
            Some("019faa1d-8971-72a3-9a72-1eeb61a00ef4:0")
        );
        assert_eq!(first.provider.as_deref(), Some("openai"));
        assert_eq!(first.model.as_deref(), Some("codex-auto-review"));
        assert_eq!(first.tokens.input, TokenField::exact(15_823));
        assert_eq!(first.tokens.cached_input, TokenField::exact(3_584));
        assert_eq!(first.tokens.output, TokenField::exact(214));
        assert_eq!(first.tokens.reasoning, TokenField::exact(110));
        assert_eq!(first.reported_total_tokens, Some(16_037));
        assert_eq!(first.project.as_deref(), Some("project"));
        assert_eq!(
            first.session_id.as_deref(),
            Some("019faa1d-8971-72a3-9a72-1eeb61a00ef4")
        );
        assert!(first.cost.is_none());
    }

    #[test]
    fn event_ids_are_stable_across_parses() {
        let ids: Vec<_> = parse_sample()
            .iter()
            .map(|draft| draft.source_event_id.clone().unwrap())
            .collect();
        let again: Vec<_> = parse_sample()
            .iter()
            .map(|draft| draft.source_event_id.clone().unwrap())
            .collect();
        assert_eq!(ids, again);
        assert_eq!(
            ids.last().map(String::as_str),
            Some("019faa1d-8971-72a3-9a72-1eeb61a00ef4:4")
        );
    }

    #[test]
    fn skips_lines_it_cannot_make_sense_of() {
        let adapter = CodexAdapter::new();
        let content = format!("not json at all\n{{\"type\":\"mystery\"}}\n{SAMPLE}");
        let input = RawSourceInput::new(content, SourceFormat::Jsonl);
        let drafts = adapter.parse(&input).unwrap();
        assert_eq!(drafts.len(), 5);
    }

    #[test]
    fn a_session_without_token_counts_yields_nothing() {
        let adapter = CodexAdapter::new();
        let content = r#"{"timestamp":"2026-05-04T23:00:43.000Z","type":"session_meta","payload":{"id":"abc","cwd":"/Users/dev/project","model_provider":"openai"}}"#;
        let input = RawSourceInput::new(content, SourceFormat::Jsonl);
        assert!(adapter.parse(&input).unwrap().is_empty());
    }

    #[test]
    fn falls_back_to_the_source_ref_when_session_meta_is_missing() {
        let adapter = CodexAdapter::new();
        let only_counts: String = SAMPLE
            .lines()
            .filter(|line| line.contains(r#""type":"token_count""#))
            .collect::<Vec<_>>()
            .join("\n");
        let input = RawSourceInput {
            source_ref: Some("fallback-ref".to_string()),
            format: SourceFormat::Jsonl,
            content: only_counts,
        };
        let drafts = adapter.parse(&input).unwrap();
        assert_eq!(drafts.len(), 5);
        assert_eq!(drafts[0].source_event_id.as_deref(), Some("fallback-ref:0"));
        assert!(drafts[0].model.is_none());
        assert!(drafts[0].provider.is_none());
        assert!(drafts[0].project.is_none());
    }

    #[test]
    fn discovers_rollouts_in_sessions_tree_and_archive() {
        let root = std::env::temp_dir().join(format!("tokens-test-{}", std::process::id()));
        let nested = root.join("sessions").join("2026").join("07").join("28");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(root.join("archived_sessions")).unwrap();
        fs::write(
            nested.join("rollout-2026-07-28T14-04-50-019faa1d-8971-72a3-9a72-1eeb61a00ef4.jsonl"),
            "",
        )
        .unwrap();
        fs::write(
            root.join("archived_sessions")
                .join("rollout-2026-02-03T13-36-43-019c24ca-e7f0-7011-8c53-1fa303b3a4a3.jsonl"),
            "",
        )
        .unwrap();
        fs::write(root.join("archived_sessions").join("notes.txt"), "").unwrap();

        let adapter = CodexAdapter::with_root(root.clone());
        let sources = adapter.discover().unwrap();
        assert_eq!(sources.len(), 2);
        assert!(sources
            .iter()
            .any(|source| source.source_ref == "019faa1d-8971-72a3-9a72-1eeb61a00ef4"));
        assert!(sources
            .iter()
            .any(|source| source.source_ref == "019c24ca-e7f0-7011-8c53-1fa303b3a4a3"));

        // A discovered source resolves back to readable content.
        let read = adapter.read(&sources[0]).unwrap();
        assert_eq!(read.source_ref, Some(sources[0].source_ref.clone()));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reports_discovery_failure_when_no_logs_exist() {
        let adapter = CodexAdapter::with_root(PathBuf::from("/definitely/not/here"));
        assert!(matches!(
            adapter.discover(),
            Err(AdapterError::Discovery { .. })
        ));
    }

    fn token_count_line(timestamp: &str, used_percent: f64) -> String {
        format!(
            r#"{{"timestamp":"{timestamp}","type":"event_msg","payload":{{"type":"token_count","info":{{"last_token_usage":{{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}},"rate_limits":{{"limit_id":"codex","primary":{{"used_percent":{used_percent},"window_minutes":10080,"resets_at":1785264899}},"secondary":null}}}}}}"#
        )
    }

    #[test]
    fn reads_the_latest_quota_snapshot_in_a_file() {
        let content = format!(
            "{}\n{}\n",
            token_count_line("2026-07-28T10:00:00.000Z", 78.0),
            token_count_line("2026-07-28T12:00:00.000Z", 93.0),
        );
        let quota = latest_quota_in(&content).unwrap();
        assert_eq!(quota.source_app, SourceApp::Codex);
        assert_eq!(quota.window_minutes, 10080);
        assert_eq!(quota.used_percent_tenths, 930);
        assert_eq!(quota.remaining_percent_tenths(), 70);
        assert_eq!(
            quota.resets_at,
            Some(chrono::DateTime::from_timestamp(1_785_264_899, 0).unwrap())
        );
        assert_eq!(
            quota.observed_at,
            chrono::DateTime::parse_from_rfc3339("2026-07-28T12:00:00.000Z")
                .unwrap()
                .to_utc()
        );
    }

    #[test]
    fn quota_ignores_lines_without_a_primary_window() {
        let content = r#"{"timestamp":"2026-07-28T12:00:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{},"rate_limits":null}}"#;
        assert!(latest_quota_in(content).is_none());
        assert!(latest_quota_in("no quota here").is_none());
    }

    #[test]
    fn quota_picks_the_freshest_event_across_files() {
        let root = std::env::temp_dir().join(format!("tokens-quota-{}", std::process::id()));
        let sessions = root.join("sessions").join("2026").join("07").join("22");
        fs::create_dir_all(&sessions).unwrap();
        // Written first, so the older mtime — but it holds the newer event,
        // because a session started days ago can still be appending.
        fs::write(
            sessions.join("rollout-2026-07-22T10-00-00-019faa1d-8971-72a3-9a72-1eeb61a00ef4.jsonl"),
            token_count_line("2026-07-29T09:00:00.000Z", 99.0),
        )
        .unwrap();
        fs::write(
            sessions.join("rollout-2026-07-22T11-00-00-019c24ca-e7f0-7011-8c53-1fa303b3a4a3.jsonl"),
            token_count_line("2026-07-28T09:00:00.000Z", 40.0),
        )
        .unwrap();

        let quotas = CodexAdapter::with_root(root.clone()).quotas().unwrap();
        assert_eq!(quotas[0].used_percent_tenths, 990);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn quota_is_none_without_logs() {
        let adapter = CodexAdapter::with_root(PathBuf::from("/definitely/not/here"));
        assert!(adapter.quotas().unwrap().is_empty());
    }

    /// A scratch `.codex` tree, cleaned up by the caller.
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("tokens-codex-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(SESSIONS_DIR)).unwrap();
        root
    }

    fn rollout_path(root: &Path, id: &str) -> PathBuf {
        root.join(SESSIONS_DIR)
            .join(format!("rollout-2026-07-28T14-04-50-{id}.jsonl"))
    }

    /// Run one delta, feeding back the checkpoints the previous one produced.
    fn tail(adapter: &CodexAdapter, carried: &mut Vec<SourceCheckpoint>) -> SourceDelta {
        let request = DeltaRequest {
            mode: SyncMode::Incremental,
            checkpoints: carried.clone(),
            now: chrono::Utc::now(),
        };
        let delta = adapter.read_delta(&request).unwrap();
        for checkpoint in &delta.checkpoints {
            carried.retain(|kept| kept.source_key != checkpoint.source_key);
            carried.push(checkpoint.clone());
        }
        delta
    }

    const ROLLOUT_ID: &str = "019faa1d-8971-72a3-9a72-1eeb61a00ef4";

    #[test]
    fn appended_events_are_read_without_rereading_the_history() {
        let root = scratch("append");
        let path = rollout_path(&root, ROLLOUT_ID);
        let lines: Vec<&str> = SAMPLE.lines().collect();
        let split = lines.len() - 2;
        fs::write(&path, format!("{}\n", lines[..split].join("\n"))).unwrap();

        let adapter = CodexAdapter::with_root(root.clone());
        let mut carried = Vec::new();
        let first = tail(&adapter, &mut carried);
        let before = first.drafts.len();
        assert!(before > 0);

        // Nothing changed: the metadata gate alone answers, and no draft is
        // produced a second time.
        assert!(tail(&adapter, &mut carried).drafts.is_empty());

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        writeln!(file, "{}", lines[split..].join("\n")).unwrap();
        drop(file);

        let appended = tail(&adapter, &mut carried);
        assert_eq!(before + appended.drafts.len(), 5);
        // Ordinals continue rather than restarting, so the appended events do
        // not collide with the ones already stored.
        assert_eq!(
            appended.drafts[0].source_event_id.as_deref(),
            Some(format!("{ROLLOUT_ID}:{before}").as_str())
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_partial_final_line_is_held_until_it_completes() {
        let root = scratch("partial");
        let path = rollout_path(&root, ROLLOUT_ID);
        let lines: Vec<&str> = SAMPLE.lines().collect();
        let whole = &lines[..lines.len() - 1];
        let torn = lines[lines.len() - 1];
        let cut = torn.len() / 2;

        fs::write(&path, format!("{}\n{}", whole.join("\n"), &torn[..cut])).unwrap();

        let adapter = CodexAdapter::with_root(root.clone());
        let mut carried = Vec::new();
        let first = tail(&adapter, &mut carried);
        // The torn line produced neither a record nor a rejection.
        assert_eq!(first.drafts.len(), 4);
        assert!(first.failures.is_empty());

        fs::write(&path, format!("{}\n{torn}\n", whole.join("\n"))).unwrap();
        let second = tail(&adapter, &mut carried);
        assert_eq!(second.drafts.len(), 1);
        assert_eq!(
            second.drafts[0].source_event_id.as_deref(),
            Some(format!("{ROLLOUT_ID}:4").as_str())
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn moving_a_rollout_into_the_archive_does_not_double_count() {
        let root = scratch("archive");
        let path = rollout_path(&root, ROLLOUT_ID);
        fs::write(&path, SAMPLE).unwrap();

        let adapter = CodexAdapter::with_root(root.clone());
        let mut carried = Vec::new();
        assert_eq!(tail(&adapter, &mut carried).drafts.len(), 5);

        // Codex archives a session by renaming it. Keyed by path this would be
        // a brand new file; keyed by inode it is the same rollout, already read.
        let archived = root.join(ARCHIVED_DIR);
        fs::create_dir_all(&archived).unwrap();
        fs::rename(
            &path,
            archived.join(path.file_name().unwrap()),
        )
        .unwrap();

        assert!(tail(&adapter, &mut carried).drafts.is_empty());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_truncated_file_restarts_instead_of_splicing() {
        let root = scratch("truncate");
        let path = rollout_path(&root, ROLLOUT_ID);
        fs::write(&path, SAMPLE).unwrap();

        let adapter = CodexAdapter::with_root(root.clone());
        let mut carried = Vec::new();
        assert_eq!(tail(&adapter, &mut carried).drafts.len(), 5);

        // A shorter file cannot be resumed from the old offset: those bytes
        // belong to different content now.
        let shortened: String = SAMPLE
            .lines()
            .take(SAMPLE.lines().count() - 2)
            .map(|line| format!("{line}\n"))
            .collect();
        fs::write(&path, &shortened).unwrap();

        let after = tail(&adapter, &mut carried);
        // Read from zero, so ordinals start at zero again and the surviving
        // events land on the identities they already had.
        assert_eq!(
            after.drafts[0].source_event_id.as_deref(),
            Some(format!("{ROLLOUT_ID}:0").as_str())
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_model_switch_survives_into_the_next_chunk() {
        let root = scratch("model");
        let path = rollout_path(&root, ROLLOUT_ID);
        let meta = format!(
            r#"{{"timestamp":"2026-07-28T19:00:00.000Z","type":"session_meta","payload":{{"id":"{ROLLOUT_ID}","cwd":"/x/project","model_provider":"openai"}}}}"#
        );
        let turn = r#"{"timestamp":"2026-07-28T19:00:01.000Z","type":"turn_context","payload":{"model":"gpt-9","cwd":"/x/project"}}"#;
        let count = r#"{"timestamp":"2026-07-28T19:00:02.000Z","type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":{"input_tokens":10,"output_tokens":2,"cached_input_tokens":0,"reasoning_output_tokens":0,"total_tokens":12}}}}"#;

        // The model line is in the first chunk only; the event in the second
        // has to inherit it from the checkpoint.
        fs::write(&path, format!("{meta}\n{turn}\n")).unwrap();
        let adapter = CodexAdapter::with_root(root.clone());
        let mut carried = Vec::new();
        assert!(tail(&adapter, &mut carried).drafts.is_empty());

        fs::write(&path, format!("{meta}\n{turn}\n{count}\n")).unwrap();
        let second = tail(&adapter, &mut carried);
        assert_eq!(second.drafts.len(), 1);
        assert_eq!(second.drafts[0].model.as_deref(), Some("gpt-9"));
        assert_eq!(second.drafts[0].provider.as_deref(), Some("openai"));
        assert_eq!(second.drafts[0].project.as_deref(), Some("project"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_allowance_comes_from_the_appended_events_themselves() {
        let root = scratch("quota");
        let path = rollout_path(&root, ROLLOUT_ID);
        // Codex stamps `rate_limits` onto its token events, so tailing the
        // appended bytes answers the allowance question for free — no second
        // pass over the freshest files.
        fs::write(
            &path,
            format!("{}\n", token_count_line("2026-07-28T19:04:56Z", 41.0)),
        )
        .unwrap();

        let adapter = CodexAdapter::with_root(root.clone());
        let mut carried = Vec::new();
        let delta = tail(&adapter, &mut carried);

        assert_eq!(delta.quotas.len(), 1);
        assert_eq!(delta.quotas[0].used_percent_tenths, 410);
        assert!(delta.replace_quotas);
        // Freshness is the source's own stamp, not the moment it was read.
        assert_eq!(delta.source_observed_at, Some(delta.quotas[0].observed_at));

        // A later event moves it, and the newer observation is the one kept.
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", token_count_line("2026-07-28T20:00:00Z", 55.5)).unwrap();
        drop(file);

        let second = tail(&adapter, &mut carried);
        assert_eq!(second.quotas[0].used_percent_tenths, 555);

        fs::remove_dir_all(&root).unwrap();
    }
}
