//! Codex adapter.
//!
//! Reads Codex's session rollouts: one JSONL file per session under
//! `~/.codex/sessions/YYYY/MM/DD/`, plus archived copies under
//! `~/.codex/archived_sessions/`. Usage arrives in `token_count` events whose
//! `last_token_usage` is one API call's worth of tokens; each becomes a draft.
//! The model comes from the enclosing `turn_context`, the provider from
//! `session_meta`. Codex reports no cost.
//!
//! Rollout files are append-only, so the n-th `token_count` event in a file is
//! a stable event identity: `{rollout_id}:{n}`. Re-imports and the
//! sessions/archived overlap deduplicate on it.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::domain::{SourceApp, TokenCounts, TokenField, UsageQuota, UsageRecordDraft};

use super::{AdapterError, DiscoveredSource, RawSourceInput, SourceAdapter};

const SESSIONS_DIR: &str = "sessions";
const ARCHIVED_DIR: &str = "archived_sessions";
const ROLLOUT_SUFFIX: &str = ".jsonl";

#[derive(Debug, Clone)]
pub struct CodexAdapter {
    /// The `.codex` directory. Overridable so tests never touch real logs.
    root: PathBuf,
}

impl CodexAdapter {
    pub const ID: &'static str = "codex";
    pub const VERSION: &'static str = "0.2.0";

    pub fn new() -> Self {
        Self { root: default_codex_dir() }
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

    fn discover(&self) -> Result<Vec<DiscoveredSource>, AdapterError> {
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

    fn read(&self, source: &DiscoveredSource) -> Result<RawSourceInput, AdapterError> {
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

    fn parse(&self, input: &RawSourceInput) -> Result<Vec<UsageRecordDraft>, AdapterError> {
        let mut session = SessionContext::default();
        let mut drafts = Vec::new();

        for line in input.content.lines() {
            // Rollouts are mostly conversation content; only these three line
            // shapes carry anything an import needs, so the rest is skipped
            // before the JSON parser ever sees it.
            let interesting = line.contains(r#""type":"session_meta""#)
                || line.contains(r#""type":"turn_context""#)
                || line.contains(r#""type":"token_count""#);
            if !interesting {
                continue;
            }

            let line: RolloutLine = match serde_json::from_str(line) {
                Ok(line) => line,
                // A truncated final line is normal in a file that is still
                // being appended to; skip it rather than failing the file.
                Err(_) => continue,
            };
            let Some(payload) = line.payload else { continue };

            match line.kind.as_str() {
                "session_meta" => {
                    if let Ok(meta) = serde_json::from_value::<SessionMeta>(payload) {
                        session.apply_meta(meta);
                    }
                }
                "turn_context" => {
                    if let Ok(turn) = serde_json::from_value::<TurnContext>(payload) {
                        session.apply_turn(turn);
                    }
                }
                "event_msg" => {
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

                    let fallback_id;
                    let rollout_id = match session.rollout_id.as_deref() {
                        Some(id) => id,
                        None => {
                            fallback_id =
                                input.source_ref.clone().unwrap_or_else(|| "unknown".to_string());
                            fallback_id.as_str()
                        }
                    };
                    let event_ordinal = drafts.len();

                    let provenance = self.provenance(input.source_ref.as_deref());
                    let mut draft = UsageRecordDraft::new(SourceApp::Codex, provenance)
                        .with_raw_timestamp(line.timestamp.unwrap_or_default())
                        .with_source_event_id(format!("{rollout_id}:{event_ordinal}"))
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
                        .with_session(session.project().as_deref(), Some(rollout_id));
                    draft.provider = session.provider.clone();
                    draft.model = session.model.clone();
                    draft.reported_total_tokens = usage.total_tokens;
                    drafts.push(draft);
                }
                _ => {}
            }
        }

        Ok(drafts)
    }

    /// Codex meters a single weekly window, so this is at most one snapshot.
    fn quotas(&self) -> Result<Vec<UsageQuota>, AdapterError> {
        // Session files are named for when a session *started*, so a stale
        // path can still hold fresh events. Modification time is the honest
        // recency signal; only the few freshest files are worth opening.
        const CANDIDATE_FILES: usize = 10;

        let mut files = self.rollout_files();
        files.sort_by_key(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok());
        let candidates: Vec<_> = files.into_iter().rev().take(CANDIDATE_FILES).collect();

        let mut freshest: Option<UsageQuota> = None;
        for path in candidates {
            let Ok(content) = fs::read_to_string(&path) else { continue };
            let Some(quota) = latest_quota_in(&content) else { continue };
            if freshest.as_ref().is_none_or(|current| quota.observed_at > current.observed_at) {
                freshest = Some(quota);
            }
        }
        Ok(freshest.into_iter().collect())
    }
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
            resets_at: chrono::DateTime::from_timestamp(window.resets_at, 0)?,
            observed_at: chrono::DateTime::parse_from_rfc3339(&line.timestamp?).ok()?.to_utc(),
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

/// Everything a rollout reveals about itself as parsing walks its lines.
#[derive(Default)]
struct SessionContext {
    rollout_id: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    cwd: Option<String>,
}

impl SessionContext {
    fn apply_meta(&mut self, meta: SessionMeta) {
        if meta.id.is_some() {
            self.rollout_id = meta.id;
        }
        if meta.model_provider.is_some() {
            self.provider = meta.model_provider;
        }
        if self.cwd.is_none() {
            self.cwd = meta.cwd;
        }
    }

    fn apply_turn(&mut self, turn: TurnContext) {
        // A session can switch models between turns; the latest context wins.
        if turn.model.is_some() {
            self.model = turn.model;
        }
        if turn.cwd.is_some() {
            self.cwd = turn.cwd;
        }
    }

    /// The project is the working directory's final component — the only part
    /// of the path a record is allowed to carry.
    fn project(&self) -> Option<String> {
        self.cwd
            .as_deref()?
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    }
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
    let Ok(entries) = fs::read_dir(dir) else { return };
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
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), |home| PathBuf::from(home).join(".codex"))
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
        assert_eq!(first.raw_timestamp.as_deref(), Some("2026-07-28T19:04:56.984Z"));
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
        assert_eq!(ids.last().map(String::as_str), Some("019faa1d-8971-72a3-9a72-1eeb61a00ef4:4"));
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
        assert!(matches!(adapter.discover(), Err(AdapterError::Discovery { .. })));
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
            chrono::DateTime::from_timestamp(1_785_264_899, 0).unwrap()
        );
        assert_eq!(
            quota.observed_at,
            chrono::DateTime::parse_from_rfc3339("2026-07-28T12:00:00.000Z").unwrap().to_utc()
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
}
