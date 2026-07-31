//! Claude Code adapter.
//!
//! Reads Claude Code's session transcripts: one JSONL file per session under
//! `~/.claude/projects/<slugified-cwd>/`. Usage lives in `assistant` lines,
//! whose `message.usage` carries input, output, cache-creation, and
//! cache-read counts separately.
//!
//! Two format subtleties shape the parse:
//!
//! - Streaming writes the same `message.id` on several lines, each a snapshot
//!   of the in-flight message. Keeping only the last snapshot per message is
//!   what stops usage being counted once per line instead of once per call.
//!   The id is globally unique, so it doubles as the deduplication identity.
//! - `input_tokens` excludes everything cached. Cache creation is input-side
//!   work (priced at a premium over plain input), so it is summed into
//!   `input`; cache read stays in `cached_input`. Claude Code reports no
//!   total and no reasoning split, and no cost.
//!
//! Allowance percentages come from a different file: `~/.claude.json`, where
//! Claude Code caches the response behind its own `/usage` command under
//! `cachedUsageUtilization`. See [`ClaudeCodeAdapter::quotas`].

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use chrono::{DateTime, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{
    SourceApp, SourceCheckpoint, TokenCounts, TokenField, UsageQuota, UsageRecordDraft,
};

use super::{
    file_identity, read_from_offset, split_complete_lines, AdapterError, DeltaRequest,
    DiscoveredSource, RawSourceInput, SourceAdapter, SourceDelta, SyncMode, WatchRoot,
};

const PROJECTS_DIR: &str = "projects";

/// Where reading one transcript resumes.
///
/// Simpler than Codex's, because Claude Code writes a globally unique
/// `message.id` on every assistant line: identity needs no ordinal, and a
/// later streaming snapshot of the same id is meant to replace the earlier
/// one rather than sit beside it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptCheckpoint {
    pub offset: u64,
    /// The trailing bytes of an incomplete line, held for the next attempt.
    #[serde(default)]
    pub partial: String,
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct ClaudeCodeAdapter {
    /// The `.claude` directory holding transcripts. Overridable so tests never
    /// touch real logs.
    root: PathBuf,
    /// `~/.claude.json` — a sibling of `root`, not a child of it, and the only
    /// place the allowance percentages live.
    config_file: PathBuf,
}

impl ClaudeCodeAdapter {
    pub const ID: &'static str = "claude_code";
    pub const VERSION: &'static str = "0.4.0";
    /// Claude Code only calls Anthropic models; the transcripts name the model
    /// but never the provider.
    const PROVIDER: &'static str = "anthropic";

    pub fn new() -> Self {
        Self {
            root: default_claude_dir(),
            config_file: default_config_file(),
        }
    }

    pub fn with_root(root: PathBuf) -> Self {
        let config_file = root.join("claude.json");
        Self { root, config_file }
    }

    /// Point both paths explicitly, for tests that exercise the quota cache.
    pub fn with_paths(root: PathBuf, config_file: PathBuf) -> Self {
        Self { root, config_file }
    }

    fn transcript_files(&self) -> Vec<PathBuf> {
        // Sessions live at `projects/<slug>/<session>.jsonl`; subagent (Task
        // tool) transcripts nest deeper, e.g. `<session>/subagents/agent-*.jsonl`.
        // Their usage appears nowhere else, so the walk is recursive.
        let mut files = Vec::new();
        let mut pending = vec![self.root.join(PROJECTS_DIR)];
        while let Some(dir) = pending.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }
}

impl Default for ClaudeCodeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceAdapter for ClaudeCodeAdapter {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn version(&self) -> &'static str {
        Self::VERSION
    }

    fn source_app(&self) -> SourceApp {
        SourceApp::ClaudeCode
    }

    /// The transcript tree, and the *parent* of `~/.claude.json`.
    ///
    /// Watching the config file itself would stop working the first time
    /// Claude Code replaced it: an atomic write creates a new file and renames
    /// it over the old one, and a watch on the old inode sees nothing. The
    /// directory sees the rename.
    fn watch_roots(&self) -> Vec<WatchRoot> {
        // The transcripts are a tree and have to be walked as one. The config
        // file's directory is watched shallowly and deliberately: it is the
        // user's home, the rename lands directly in it, and nothing nested
        // under it belongs to this source.
        let mut roots = vec![WatchRoot::tree(self.root.join(PROJECTS_DIR))];
        if let Some(parent) = self.config_file.parent() {
            roots.push(WatchRoot::shallow(parent.to_path_buf()));
        }
        roots
    }

    fn read_delta(&self, request: &DeltaRequest) -> Result<SourceDelta, AdapterError> {
        let quotas = self.quotas_at(request.now);
        let source_observed_at = quotas.iter().map(|quota| quota.observed_at).max();

        if request.mode == SyncMode::QuotaOnly {
            return Ok(SourceDelta {
                replace_quotas: !quotas.is_empty(),
                quotas,
                source_observed_at,
                ..SourceDelta::default()
            });
        }

        // Recursive, and re-walked every pass: a Task tool subagent creates a
        // nested directory mid-session, and its usage appears nowhere else.
        let files = self.transcript_files();
        if files.is_empty() {
            return Err(AdapterError::Discovery {
                adapter: Self::ID,
                reason: "no transcripts found".to_string(),
            });
        }

        let mut delta = SourceDelta {
            replace_quotas: !quotas.is_empty(),
            quotas,
            source_observed_at,
            ..SourceDelta::default()
        };
        let base = self.root.join(PROJECTS_DIR);

        for path in files {
            let Some(source_key) = file_identity(&path) else {
                continue;
            };
            let stored: TranscriptCheckpoint = request.resume(&source_key).unwrap_or_default();

            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            let size = metadata.len();
            if size == stored.size && stored.offset == size && stored.partial.is_empty() {
                continue;
            }

            let (appended, read_to, restarted) = match read_from_offset(&path, stored.offset) {
                Ok(result) => result,
                Err(error) => {
                    delta.failures.push(format!(
                        "a Claude Code transcript could not be read: {error}"
                    ));
                    continue;
                }
            };

            let carried = if restarted {
                String::new()
            } else {
                stored.partial
            };
            let text = format!("{carried}{appended}");
            let (complete, partial) = split_complete_lines(&text);

            let source_ref = path
                .strip_prefix(&base)
                .ok()
                .and_then(|relative| relative.to_str())
                .map(|relative| relative.trim_end_matches(".jsonl").to_string())
                .unwrap_or_else(|| source_key.clone());

            delta
                .drafts
                .extend(parse_transcript(self, &source_ref, complete));
            delta.checkpoints.push(SourceCheckpoint {
                adapter_id: Self::ID.to_string(),
                source_key,
                payload: serde_json::to_value(TranscriptCheckpoint {
                    offset: read_to,
                    partial: partial.to_string(),
                    size,
                })
                .unwrap_or_default(),
            });
        }

        Ok(delta)
    }
}

impl ClaudeCodeAdapter {
    /// Locate this adapter's transcripts. Kept for tests and diagnostics; the
    /// running application reaches sources through [`SourceAdapter::read_delta`].
    pub fn discover(&self) -> Result<Vec<DiscoveredSource>, AdapterError> {
        let base = self.root.join(PROJECTS_DIR);
        let files = self.transcript_files();
        if files.is_empty() {
            return Err(AdapterError::Discovery {
                adapter: Self::ID,
                reason: format!("no transcripts found under {}", self.root.display()),
            });
        }

        // The ref is the path relative to the projects dir (no extension) —
        // subagent transcripts share the `agent-*` naming pattern, so the
        // nesting is what keeps refs unique. The path never leaves the adapter.
        Ok(files
            .into_iter()
            .filter_map(|path| {
                let relative = path.strip_prefix(&base).ok()?.to_str()?.to_string();
                let source_ref = relative.trim_end_matches(".jsonl").to_string();
                Some(DiscoveredSource {
                    label: source_ref.clone(),
                    source_ref,
                    format: super::SourceFormat::Jsonl,
                })
            })
            .collect())
    }

    pub fn read(&self, source: &DiscoveredSource) -> Result<RawSourceInput, AdapterError> {
        let path = self
            .root
            .join(PROJECTS_DIR)
            .join(format!("{}.jsonl", source.source_ref));
        let content = fs::read_to_string(&path).map_err(|error| AdapterError::Unreadable {
            adapter: Self::ID,
            reason: format!("{}: {error}", path.display()),
        })?;

        Ok(RawSourceInput::from_source(source, content))
    }

    pub fn parse(&self, input: &RawSourceInput) -> Result<Vec<UsageRecordDraft>, AdapterError> {
        let source_ref = input.source_ref.clone().unwrap_or_default();
        Ok(parse_transcript(self, &source_ref, &input.content))
    }

    /// Allowance percentages, read from Claude Code's own cache of them.
    ///
    /// Anthropic publishes no local quota file and no documented API for
    /// subscription usage, but Claude Code's `/usage` command has to get the
    /// numbers from somewhere: it calls an undocumented OAuth endpoint and
    /// writes the reply into `~/.claude.json` under `cachedUsageUtilization`.
    /// Reading that cache gives the same figures `/usage` prints — the
    /// server's own accounting, not an estimate — while staying a local-file
    /// read, which is the whole premise of this app. It also means no token
    /// handling and no traffic that could consume the account's rate limit.
    ///
    /// The cache reports a `five_hour` session window and a `seven_day` week,
    /// each with `utilization` and `resets_at`; both become quotas, so the
    /// interface can show whichever binds first. `fetchedAtMs` is the snapshot
    /// age. A window whose `resets_at` has passed is dropped: its percentage
    /// describes a window that no longer exists.
    ///
    /// A `resets_at` of `null` means something different and is kept: the
    /// session window is rolling, so between sessions Claude reports zero used
    /// and no reset, because no window is running. That is current information —
    /// the whole session is available — and dropping it would leave the session
    /// unaccounted for whenever it matters least to hide it.
    ///
    /// The session window gets one further step. Because Claude Code only
    /// rewrites the cache while it is running, an idle machine holds a session
    /// `resets_at` that has already passed — and simply dropping it makes the
    /// window disappear from the interface just as a whole fresh allowance
    /// becomes available. The transcripts still hold the timeline, so it is
    /// rebuilt from them; see [`ClaudeCodeAdapter::derived_session_window`].
    ///
    /// When the cache is missing or entirely stale — a fresh install, or a
    /// version that does not write it — a live 429 transcript line still
    /// proves an allowance is exhausted, and that is used instead.
    pub fn quotas(&self) -> Result<Vec<UsageQuota>, AdapterError> {
        Ok(self.quotas_at(Utc::now()))
    }
}

impl ClaudeCodeAdapter {
    /// The quota logic, with the clock as an argument so it can be tested
    /// without pinning fixtures to whatever today happens to be.
    fn quotas_at(&self, now: DateTime<Utc>) -> Vec<UsageQuota> {
        let cached = self.cached_utilization().unwrap_or_default();
        let mut live: Vec<UsageQuota> = cached
            .iter()
            .filter(|quota| quota.is_current_at(now))
            .cloned()
            .collect();

        // The session window is the one that expires while nobody is looking.
        // Claude Code only rewrites its cache while it is running, so an idle
        // machine keeps a `resets_at` that has long since passed — and the
        // filter above quite correctly drops it, because that percentage
        // describes a window that no longer exists.
        //
        // Dropping it entirely is what does the damage: the batch then stops
        // reporting the window, `replace_quotas` deletes the stored row, and
        // the session disappears from the interface at the exact moment a
        // whole fresh allowance became available. The transcripts still know
        // what happened, so they are asked instead.
        if !live.iter().any(is_session_window) {
            if let Some(derived) = cached
                .iter()
                .find(|quota| is_session_window(quota))
                .and_then(|expired| self.derived_session_window(expired, now))
            {
                // Ahead of the week that contains it, matching the order
                // `cached_utilization` produces and the interface expects.
                live.insert(0, derived);
            }
        }

        if !live.is_empty() {
            return live;
        }

        self.limit_hit_quota(now).into_iter().collect()
    }

    /// The session window as the transcripts describe it, once the cache's own
    /// has expired.
    ///
    /// Only the *percentage* died with the old window. Its `resets_at` is still
    /// a fact — the instant that window ended — and from there the timeline is
    /// fully derivable locally, because a rolling window opens on the first
    /// request after a reset and then runs its full length whether or not the
    /// work continues. Message timestamps are exact and, unlike the cache, do
    /// not need another process to be running to stay true.
    ///
    /// The result always reports zero used. That is not a guess: a rolling
    /// window genuinely opens empty, and it is the honest confirmed value for
    /// the instant it opened. What has been spent into it since is the
    /// projection layer's business, from records this adapter cannot see.
    fn derived_session_window(
        &self,
        expired: &UsageQuota,
        now: DateTime<Utc>,
    ) -> Option<UsageQuota> {
        let ended_at = expired.resets_at?;
        let window = chrono::Duration::minutes(i64::from(SESSION_WINDOW_MINUTES));

        let mut times = self.activity_since(ended_at);
        times.sort_unstable();

        // Windows tile forward from the end of the expired one. Walking rather
        // than dividing is what keeps a long idle gap honest: a window only
        // exists if a request actually opened it.
        let Some(&first) = times.first() else {
            return Some(idle_session(expired.observed_at));
        };
        let mut anchor = first;
        while anchor + window <= now {
            match times.iter().find(|at| **at >= anchor + window) {
                Some(next) => anchor = *next,
                None => return Some(idle_session(now)),
            }
        }

        Some(UsageQuota {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: SESSION_WINDOW_MINUTES,
            used_percent_tenths: 0,
            resets_at: Some(anchor + window),
            // Zero used was true when the window opened, not now.
            observed_at: anchor,
        })
    }

    /// Message timestamps at or after `since`, from the freshest transcripts.
    ///
    /// Scanned as text rather than parsed as JSON: only the timestamp is
    /// wanted, every line carries one in the same shape, and this runs on the
    /// quota lane once a minute.
    fn activity_since(&self, since: DateTime<Utc>) -> Vec<DateTime<Utc>> {
        let mut files = self.transcript_files();
        files.sort_by_key(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok());

        let mut times = Vec::new();
        for path in files.iter().rev().take(FRESHEST_FILES_FOR_QUOTA) {
            // A file untouched since before the cutoff cannot hold a message
            // after it, and reading it would be pure cost.
            let untouched = fs::metadata(path)
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| DateTime::<Utc>::from(modified) < since);
            if untouched {
                continue;
            }
            let Ok(content) = fs::read_to_string(path) else {
                continue;
            };
            times.extend(
                content
                    .lines()
                    .filter_map(line_timestamp)
                    .filter(|at| *at >= since),
            );
        }
        times
    }

    /// Every window in `~/.claude.json`'s `cachedUsageUtilization`, unfiltered.
    fn cached_utilization(&self) -> Option<Vec<UsageQuota>> {
        let content = fs::read_to_string(&self.config_file).ok()?;
        let config: ClaudeConfig = serde_json::from_str(&content).ok()?;
        let cache = config.cached_usage_utilization?;
        let observed_at = DateTime::from_timestamp_millis(cache.fetched_at_ms?)?;

        let utilization = cache.utilization?;
        Some(
            [
                (None, SESSION_WINDOW_MINUTES, utilization.five_hour),
                (None, WEEK_WINDOW_MINUTES, utilization.seven_day),
                // Per-model weekly caps exist on some plans and are null on
                // others; when present they bind just as hard as the rest.
                // They share the week's length, so only the label tells them
                // apart from the overall cap — and from each other.
                (
                    Some("Opus"),
                    WEEK_WINDOW_MINUTES,
                    utilization.seven_day_opus,
                ),
                (
                    Some("Sonnet"),
                    WEEK_WINDOW_MINUTES,
                    utilization.seven_day_sonnet,
                ),
            ]
            .into_iter()
            .filter_map(|(label, window_minutes, window)| {
                let window = window?;
                Some(UsageQuota {
                    source_app: SourceApp::ClaudeCode,
                    label: label.map(str::to_string),
                    window_minutes,
                    used_percent_tenths: percent_to_tenths(window.utilization?),
                    // Absent means no window is running; present but unreadable
                    // means the snapshot cannot be trusted, so the window goes.
                    resets_at: match window.resets_at.as_deref() {
                        Some(raw) => Some(DateTime::parse_from_rfc3339(raw).ok()?.to_utc()),
                        None => None,
                    },
                    observed_at,
                })
            })
            .collect(),
        )
    }

    /// The fallback: a 429 line proves the window it names is exhausted.
    ///
    /// `You've hit your session limit · resets 7:20pm (America/Panama)`. While
    /// the newest such line is still inside its window that is the quota, 100%
    /// used. Once the stated reset passes the line is history, not state.
    fn limit_hit_quota(&self, now: DateTime<Utc>) -> Option<UsageQuota> {
        let mut files = self.transcript_files();
        files.sort_by_key(|path| fs::metadata(path).and_then(|meta| meta.modified()).ok());

        for path in files.iter().rev().take(FRESHEST_FILES_FOR_QUOTA) {
            let Ok(content) = fs::read_to_string(path) else {
                continue;
            };
            for line in content.lines().rev() {
                if !line.contains(r#""error":"rate_limit""#) {
                    continue;
                }
                let Ok(parsed) = serde_json::from_str::<LimitLine>(line) else {
                    continue;
                };
                if let Some(quota) = quota_from_limit_line(&parsed, now) {
                    return Some(quota);
                }
            }
        }
        None
    }
}

/// Turn assistant lines into drafts, keeping one per `message.id`.
///
/// Streaming writes the same id on several lines, each a snapshot of the
/// in-flight message, so the last one within a chunk wins here. Across chunks
/// the repository does the same job: identity is the id, so a later snapshot
/// updates the stored row rather than adding to it, and the counts converge on
/// the final ones without any of the interim values surviving.
fn parse_transcript(
    adapter: &ClaudeCodeAdapter,
    source_ref: &str,
    content: &str,
) -> Vec<UsageRecordDraft> {
    let mut drafts: Vec<UsageRecordDraft> = Vec::new();
    let mut positions: HashMap<String, usize> = HashMap::new();

    for line in content.lines() {
        if !line.contains(r#""type":"assistant""#) {
            continue;
        }
        let line: TranscriptLine = match serde_json::from_str(line) {
            Ok(line) => line,
            // A line this build cannot make sense of is skipped rather than
            // failing the transcript around it.
            Err(_) => continue,
        };
        let Some(message) = line.message else {
            continue;
        };
        let Some(usage) = message.usage else { continue };

        let provenance = adapter.provenance(Some(source_ref));
        let mut draft = UsageRecordDraft::new(SourceApp::ClaudeCode, provenance)
            .with_raw_timestamp(line.timestamp.unwrap_or_default())
            .with_source_event_id(message.id.clone().or(line.uuid).unwrap_or_default())
            .with_tokens(TokenCounts {
                input: sum_as_exact(usage.input_tokens, usage.cache_creation_input_tokens),
                output: usage
                    .output_tokens
                    .map_or_else(TokenField::unknown, TokenField::exact),
                cached_input: usage
                    .cache_read_input_tokens
                    .map_or_else(TokenField::unknown, TokenField::exact),
                reasoning: TokenField::unknown(),
            });
        draft.provider = Some(ClaudeCodeAdapter::PROVIDER.to_string());
        draft.model = message.model;
        draft.project = line.cwd.as_deref().and_then(project_name);
        draft.session_id = line.session_id;

        match positions.get(draft.source_event_id.as_deref().unwrap_or_default()) {
            Some(&position) => drafts[position] = draft,
            None => {
                let key = draft.source_event_id.clone().unwrap_or_default();
                positions.insert(key, drafts.len());
                drafts.push(draft);
            }
        }
    }

    drafts
}

const SESSION_WINDOW_MINUTES: u32 = 300;
const WEEK_WINDOW_MINUTES: u32 = 10_080;

/// The rolling session pool, as opposed to the week or a per-model cap. Length
/// alone is not enough — the per-model caps share the week's — but for the
/// session the pairing of length and no label is unambiguous.
fn is_session_window(quota: &UsageQuota) -> bool {
    quota.window_minutes == SESSION_WINDOW_MINUTES && quota.label.is_none()
}

/// No session window running: the whole allowance is available and there is no
/// reset to count down to. The same shape Claude Code itself writes between
/// sessions, so nothing downstream has to learn a second way to say it.
///
/// `observed_at` is inherited from the cache this was derived against rather
/// than set to now. `read_delta` reports the newest observation as the whole
/// source's freshness, and stamping a derived row with the current time would
/// have the interface claim Claude Code had just spoken when in fact its
/// percentages are hours old.
fn idle_session(observed_at: DateTime<Utc>) -> UsageQuota {
    UsageQuota {
        source_app: SourceApp::ClaudeCode,
        label: None,
        window_minutes: SESSION_WINDOW_MINUTES,
        used_percent_tenths: 0,
        resets_at: None,
        observed_at,
    }
}

/// The `timestamp` field of one transcript line, without parsing the rest.
fn line_timestamp(line: &str) -> Option<DateTime<Utc>> {
    const KEY: &str = r#""timestamp":""#;
    let rest = &line[line.find(KEY)? + KEY.len()..];
    let raw = &rest[..rest.find('"')?];
    DateTime::parse_from_rfc3339(raw).ok().map(|at| at.to_utc())
}

/// The cache states percentages as JSON numbers (`51`, `85.575`). This is the
/// one place a float is allowed in, and it converts straight to the integer
/// tenths the rest of the app carries, half-up and clamped to a real window.
fn percent_to_tenths(percent: f64) -> u16 {
    if !percent.is_finite() || percent <= 0.0 {
        return 0;
    }
    (percent * 10.0).round().min(1_000.0) as u16
}

/// Only the freshest transcripts can hold a live 429; the rest are history.
const FRESHEST_FILES_FOR_QUOTA: usize = 10;

/// Interpret one `error: "rate_limit"` line against the current time.
fn quota_from_limit_line(line: &LimitLine, now: DateTime<Utc>) -> Option<UsageQuota> {
    if line.error.as_deref() != Some("rate_limit") {
        return None;
    }
    let observed_at = DateTime::parse_from_rfc3339(line.timestamp.as_deref()?)
        .ok()?
        .to_utc();
    let text = line
        .message
        .as_ref()?
        .content
        .as_ref()?
        .iter()
        .find_map(|c| c.text.as_deref())?;

    let (head, tail) = text.split_once(" · resets ")?;
    let window_minutes = if head.contains("session limit") {
        300
    } else if head.contains("weekly limit") {
        10_080
    } else {
        return None;
    };
    let (time, zone) = tail.split_once(" (")?;
    let time = parse_reset_time(time)?;
    let zone: chrono_tz::Tz = zone.trim_end_matches(')').parse().ok()?;

    // The message names a wall-clock time, not a date: it is the next such
    // time after the error, in the machine's zone.
    let observed_local = observed_at.with_timezone(&zone);
    let mut resets_at = zone
        .from_local_datetime(&observed_local.date_naive().and_time(time))
        .earliest()?;
    if resets_at <= observed_local {
        resets_at = zone
            .from_local_datetime(
                &(observed_local.date_naive() + chrono::Duration::days(1)).and_time(time),
            )
            .earliest()?;
    }
    let resets_at = resets_at.with_timezone(&Utc);

    // A window that has already reset says nothing about the present.
    if resets_at <= now {
        return None;
    }

    Some(UsageQuota {
        source_app: SourceApp::ClaudeCode,
        label: None,
        window_minutes,
        used_percent_tenths: 1_000,
        resets_at: Some(resets_at),
        observed_at,
    })
}

/// "7:20pm" / "1:10am" — including the minute-less "7pm" the message
/// sometimes uses for round hours.
fn parse_reset_time(text: &str) -> Option<NaiveTime> {
    let text = text.trim();
    let (clock, meridiem) = text.split_at(text.len().checked_sub(2)?);
    if !matches!(meridiem, "am" | "pm") {
        return None;
    }
    let (hour, minute): (u32, u32) = match clock.split_once(':') {
        Some((hour, minute)) => (hour.trim().parse().ok()?, minute.trim().parse().ok()?),
        None => (clock.trim().parse().ok()?, 0),
    };
    if !(1..=12).contains(&hour) || minute > 59 {
        return None;
    }
    let hour = if meridiem == "am" {
        hour % 12
    } else {
        hour % 12 + 12
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// Both counts are uncached input-side work; summing keeps one exact field.
fn sum_as_exact(input: Option<u64>, cache_creation: Option<u64>) -> TokenField {
    match (input, cache_creation) {
        (None, None) => TokenField::unknown(),
        (input, cache_creation) => TokenField::exact(
            input
                .unwrap_or(0)
                .saturating_add(cache_creation.unwrap_or(0)),
        ),
    }
}

/// The project is the working directory's final component — the only part of
/// the path a record is allowed to carry.
fn project_name(cwd: &str) -> Option<String> {
    cwd.rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

#[derive(Deserialize)]
struct TranscriptLine {
    timestamp: Option<String>,
    uuid: Option<String>,
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
    cwd: Option<String>,
    message: Option<AssistantMessage>,
}

#[derive(Deserialize)]
struct AssistantMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<MessageUsage>,
}

#[derive(Deserialize)]
struct MessageUsage {
    input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

/// `~/.claude.json`, of which only the usage cache is of interest. Everything
/// is optional: this is another app's private file, and every field in it can
/// disappear in a release without warning.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeConfig {
    cached_usage_utilization: Option<CachedUsageUtilization>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CachedUsageUtilization {
    /// Unix milliseconds — when Claude Code last fetched these numbers.
    fetched_at_ms: Option<i64>,
    utilization: Option<Utilization>,
}

#[derive(Deserialize)]
struct Utilization {
    five_hour: Option<UtilizationWindow>,
    seven_day: Option<UtilizationWindow>,
    seven_day_opus: Option<UtilizationWindow>,
    seven_day_sonnet: Option<UtilizationWindow>,
}

#[derive(Deserialize)]
struct UtilizationWindow {
    /// Percent of the window already consumed, 0–100.
    utilization: Option<f64>,
    resets_at: Option<String>,
}

/// The 429 error shape — unrelated to usage lines, so it gets its own
/// structs rather than complicating the message ones.
#[derive(Deserialize)]
struct LimitLine {
    timestamp: Option<String>,
    error: Option<String>,
    message: Option<LimitMessage>,
}

#[derive(Deserialize)]
struct LimitMessage {
    content: Option<Vec<LimitContent>>,
}

#[derive(Deserialize)]
struct LimitContent {
    text: Option<String>,
}

fn default_claude_dir() -> PathBuf {
    home().join(".claude")
}

fn default_config_file() -> PathBuf {
    home().join(".claude.json")
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::SourceFormat;

    const SAMPLE: &str = include_str!("../../fixtures/samples/claude-code-transcript.jsonl");

    fn parse_sample() -> Vec<UsageRecordDraft> {
        let adapter = ClaudeCodeAdapter::new();
        let input = RawSourceInput {
            source_ref: Some("3cff86d3-6a42-4a80-9f50-b41e10ffebbc".to_string()),
            format: SourceFormat::Jsonl,
            content: SAMPLE.to_string(),
        };
        adapter.parse(&input).unwrap()
    }

    #[test]
    fn keeps_only_the_final_snapshot_per_message() {
        // The sample holds four assistant lines but only two messages:
        // msg_011CdWwgBPFQ2cBeLZtsk3Mp appears three times, identical each time.
        let drafts = parse_sample();
        assert_eq!(drafts.len(), 2);
        assert_eq!(
            drafts[0].source_event_id.as_deref(),
            Some("msg_011CdWwgBPFQ2cBeLZtsk3Mp")
        );
        assert_eq!(
            drafts[1].source_event_id.as_deref(),
            Some("msg_011CdWwo4K3Z5cPmcgm6ZUGu")
        );
    }

    #[test]
    fn maps_usage_into_the_record_categories() {
        let drafts = parse_sample();
        let first = &drafts[0];

        assert_eq!(first.source_app, SourceApp::ClaudeCode);
        // input 2 + cache creation 11,184 — uncached input-side work together.
        assert_eq!(first.tokens.input, TokenField::exact(11_186));
        assert_eq!(first.tokens.cached_input, TokenField::exact(12_738));
        assert_eq!(first.tokens.output, TokenField::exact(374));
        assert_eq!(first.tokens.reasoning, TokenField::unknown());
        // The last snapshot's timestamp wins.
        assert_eq!(
            first.raw_timestamp.as_deref(),
            Some("2026-07-29T20:00:53.212Z")
        );
        assert_eq!(first.provider.as_deref(), Some("anthropic"));
        assert_eq!(first.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(first.project.as_deref(), Some("project"));
        assert_eq!(
            first.session_id.as_deref(),
            Some("3cff86d3-6a42-4a80-9f50-b41e10ffebbc")
        );
        assert!(first.reported_total_tokens.is_none());
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
    }

    #[test]
    fn skips_lines_it_cannot_make_sense_of() {
        let adapter = ClaudeCodeAdapter::new();
        let content = format!("not json\n{{\"type\":\"user\"}}\n{SAMPLE}");
        let input = RawSourceInput::new(content, SourceFormat::Jsonl);
        assert_eq!(adapter.parse(&input).unwrap().len(), 2);
    }

    #[test]
    fn a_transcript_without_usage_yields_nothing() {
        let adapter = ClaudeCodeAdapter::new();
        let content = r#"{"type":"user","timestamp":"2026-07-29T10:00:00Z","uuid":"u","message":{"role":"user","content":"hi"}}"#;
        let input = RawSourceInput::new(content, SourceFormat::Jsonl);
        assert!(adapter.parse(&input).unwrap().is_empty());
    }

    #[test]
    fn discovers_transcripts_across_project_dirs() {
        let root = std::env::temp_dir().join(format!("tokens-claude-{}", std::process::id()));
        let project = root.join("projects").join("-Users-dev-project");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("3cff86d3-6a42-4a80-9f50-b41e10ffebbc.jsonl"),
            "",
        )
        .unwrap();
        fs::write(project.join("notes.txt"), "").unwrap();
        // Subagent transcripts nest under the session directory.
        let subagents = project
            .join("3cff86d3-6a42-4a80-9f50-b41e10ffebbc")
            .join("subagents");
        fs::create_dir_all(&subagents).unwrap();
        fs::write(subagents.join("agent-a1e82699a9b25696f.jsonl"), "").unwrap();
        fs::create_dir_all(root.join("not-projects")).unwrap();

        let adapter = ClaudeCodeAdapter::with_root(root.clone());
        let sources = adapter.discover().unwrap();
        let refs: Vec<_> = sources
            .iter()
            .map(|source| source.source_ref.as_str())
            .collect();
        // Path ordering is component-wise: the session directory sorts ahead
        // of the `.jsonl` sibling whose name it prefixes.
        assert_eq!(
            refs,
            vec![
                "-Users-dev-project/3cff86d3-6a42-4a80-9f50-b41e10ffebbc/subagents/agent-a1e82699a9b25696f",
                "-Users-dev-project/3cff86d3-6a42-4a80-9f50-b41e10ffebbc",
            ]
        );

        for source in &sources {
            let read = adapter.read(source).unwrap();
            assert_eq!(read.source_ref, Some(source.source_ref.clone()));
        }

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn reports_discovery_failure_when_no_logs_exist() {
        let adapter = ClaudeCodeAdapter::with_root(PathBuf::from("/definitely/not/here"));
        assert!(matches!(
            adapter.discover(),
            Err(AdapterError::Discovery { .. })
        ));
    }

    fn limit_line(text: &str) -> LimitLine {
        serde_json::from_value(serde_json::json!({
            "timestamp": "2026-07-13T20:46:48.803Z",
            "error": "rate_limit",
            "message": { "content": [{ "type": "text", "text": text }] },
        }))
        .unwrap()
    }

    fn fixed_now() -> DateTime<Utc> {
        // Two hours after the error below, still inside the session window.
        DateTime::parse_from_rfc3339("2026-07-13T22:46:48Z")
            .unwrap()
            .to_utc()
    }

    #[test]
    fn a_live_429_becomes_an_exhausted_quota() {
        let line = limit_line("You've hit your session limit · resets 7:20pm (America/Panama)");
        let quota = quota_from_limit_line(&line, fixed_now()).unwrap();

        assert_eq!(quota.source_app, SourceApp::ClaudeCode);
        assert_eq!(quota.window_minutes, 300);
        assert_eq!(quota.used_percent_tenths, 1_000);
        assert_eq!(
            quota.observed_at.to_rfc3339(),
            "2026-07-13T20:46:48.803+00:00"
        );
        // 7:20pm in America/Panama (UTC-5) on the error's own date.
        assert_eq!(
            quota.resets_at.unwrap().to_rfc3339(),
            "2026-07-14T00:20:00+00:00"
        );
    }

    #[test]
    fn a_reset_time_in_the_past_rolls_to_the_next_day() {
        // 1:10pm is before the 3:46pm local error time, so the reset must be
        // the following day.
        let line = limit_line("You've hit your session limit · resets 1:10pm (America/Panama)");
        let quota = quota_from_limit_line(&line, fixed_now()).unwrap();
        assert_eq!(
            quota.resets_at.unwrap().to_rfc3339(),
            "2026-07-14T18:10:00+00:00"
        );
    }

    #[test]
    fn minute_less_round_hours_parse() {
        assert_eq!(parse_reset_time("7pm"), NaiveTime::from_hms_opt(19, 0, 0));
        assert_eq!(parse_reset_time("12am"), NaiveTime::from_hms_opt(0, 0, 0));
        assert_eq!(parse_reset_time("12pm"), NaiveTime::from_hms_opt(12, 0, 0));
        assert_eq!(parse_reset_time("nonsense"), None);
    }

    #[test]
    fn an_expired_window_is_history_not_state() {
        // Now is long past the reset: nothing about the present can be said.
        let line = limit_line("You've hit your session limit · resets 7:20pm (America/Panama)");
        let now = DateTime::parse_from_rfc3339("2026-07-15T00:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(quota_from_limit_line(&line, now), None);
    }

    #[test]
    fn non_limit_lines_produce_no_quota() {
        let line = limit_line("Some ordinary error text");
        assert_eq!(quota_from_limit_line(&line, fixed_now()), None);
    }

    /// The cache as Claude Code actually writes it, trimmed to the fields
    /// this adapter reads plus a few it must ignore.
    fn config_json(fetched_at_ms: i64, five_hour_resets: &str, seven_day_resets: &str) -> String {
        serde_json::json!({
            "numStartups": 412,
            "cachedUsageUtilization": {
                "fetchedAtMs": fetched_at_ms,
                "accountUuid": "882cd4f7-2ebd-4f5c-90b2-daf80eeaf3bc",
                "utilization": {
                    "five_hour": {
                        "utilization": 51,
                        "resets_at": five_hour_resets,
                        "limit_dollars": null,
                    },
                    "seven_day": {
                        "utilization": 38.5,
                        "resets_at": seven_day_resets,
                    },
                    "seven_day_opus": null,
                    "seven_day_sonnet": null,
                    "iguana_necktie": null,
                    "extra_usage": { "is_enabled": true, "utilization": 85.575 },
                    "limits": [{ "kind": "session", "percent": 51 }],
                },
            },
        })
        .to_string()
    }

    fn adapter_with_config(dir: &std::path::Path, config: &str) -> ClaudeCodeAdapter {
        fs::create_dir_all(dir.join("projects")).unwrap();
        let config_file = dir.join("claude.json");
        fs::write(&config_file, config).unwrap();
        ClaudeCodeAdapter::with_paths(dir.to_path_buf(), config_file)
    }

    /// Between the two resets in `config_json`, so the session window is live
    /// and the weekly one is too.
    fn before_both_resets() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-29T22:00:00Z")
            .unwrap()
            .to_utc()
    }

    #[test]
    fn reads_every_live_window_from_the_usage_cache() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-cache-{}", std::process::id()));
        let config = config_json(
            1_785_359_289_614,
            "2026-07-29T23:50:00.308400+00:00",
            "2026-07-31T23:00:00.308427+00:00",
        );
        let adapter = adapter_with_config(&dir, &config);

        let quotas = adapter.quotas_at(before_both_resets());
        assert_eq!(quotas.len(), 2);

        let session = &quotas[0];
        assert_eq!(session.source_app, SourceApp::ClaudeCode);
        assert_eq!(session.window_minutes, 300);
        assert_eq!(session.used_percent_tenths, 510);
        assert_eq!(session.remaining_percent_tenths(), 490);
        assert_eq!(
            session.observed_at.to_rfc3339(),
            "2026-07-29T21:08:09.614+00:00"
        );

        let week = &quotas[1];
        assert_eq!(week.window_minutes, 10_080);
        // 38.5% survives as tenths rather than rounding to a whole percent.
        assert_eq!(week.used_percent_tenths, 385);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_session_between_windows_is_reported_as_untouched_not_dropped() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-idle-{}", std::process::id()));
        // What Claude writes when no session is running: nothing used, and no
        // reset, because the rolling window has not started.
        let mut config: serde_json::Value = serde_json::from_str(&config_json(
            1_785_359_289_614,
            "2026-07-29T23:50:00.308400+00:00",
            "2026-07-31T23:00:00.308427+00:00",
        ))
        .unwrap();
        config["cachedUsageUtilization"]["utilization"]["five_hour"] =
            serde_json::json!({ "utilization": 0, "resets_at": null, "limit_dollars": null });
        let adapter = adapter_with_config(&dir, &config.to_string());

        let quotas = adapter.quotas_at(before_both_resets());
        assert_eq!(quotas.len(), 2);

        let session = &quotas[0];
        assert_eq!(session.window_minutes, 300);
        assert_eq!(session.resets_at, None);
        assert_eq!(session.remaining_percent_tenths(), 1_000);
        // Still current: it says the whole session is available right now.
        assert!(session.is_current_at(before_both_resets()));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn per_model_weekly_caps_are_labelled_so_they_can_be_told_apart() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-models-{}", std::process::id()));
        // Opus and Sonnet share the week's length with the overall cap, so
        // without a label three rows would claim to measure the same thing.
        let config = config_json(
            1_785_359_289_614,
            "2026-07-29T23:50:00.308400+00:00",
            "2026-07-31T23:00:00.308427+00:00",
        )
        .replace(
            r#""seven_day_opus":null"#,
            r#""seven_day_opus":{"utilization":12.5,"resets_at":"2026-07-31T23:00:00.308427+00:00"}"#,
        )
        .replace(
            r#""seven_day_sonnet":null"#,
            r#""seven_day_sonnet":{"utilization":4,"resets_at":"2026-07-31T23:00:00.308427+00:00"}"#,
        );
        let adapter = adapter_with_config(&dir, &config);

        let quotas = adapter.quotas_at(before_both_resets());
        let labels: Vec<_> = quotas.iter().map(|quota| quota.label.as_deref()).collect();
        assert_eq!(labels, vec![None, None, Some("Opus"), Some("Sonnet")]);

        let opus = &quotas[2];
        assert_eq!(opus.window_minutes, 10_080);
        assert_eq!(opus.used_percent_tenths, 125);

        fs::remove_dir_all(&dir).unwrap();
    }

    /// A transcript holding nothing but message timestamps, which is all the
    /// session timeline is derived from.
    fn write_transcript(dir: &std::path::Path, name: &str, timestamps: &[&str]) {
        let project = dir.join("projects").join("-Users-dev-project");
        fs::create_dir_all(&project).unwrap();
        let body: String = timestamps
            .iter()
            .map(|at| format!("{{\"type\":\"assistant\",\"timestamp\":\"{at}\"}}\n"))
            .collect();
        fs::write(project.join(name), body).unwrap();
    }

    /// The standard fixture: session reset 2026-07-29T23:50, week two days on.
    fn expired_session_adapter(dir: &std::path::Path) -> ClaudeCodeAdapter {
        adapter_with_config(
            dir,
            &config_json(
                1_785_359_289_614,
                "2026-07-29T23:50:00.308400+00:00",
                "2026-07-31T23:00:00.308427+00:00",
            ),
        )
    }

    fn instant(raw: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(raw).unwrap().to_utc()
    }

    #[test]
    fn an_expired_percentage_is_dropped_but_the_window_is_not() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-stale-{}", std::process::id()));
        let adapter = expired_session_adapter(&dir);

        // An hour after the session window rolled over, with nothing done
        // since: the cached 51% describes a window that no longer exists, but
        // the session itself is whole and must still be reported.
        let quotas = adapter.quotas_at(instant("2026-07-30T01:00:00Z"));
        assert_eq!(quotas.len(), 2);

        let session = &quotas[0];
        assert!(is_session_window(session));
        assert_eq!(session.used_percent_tenths, 0);
        assert_eq!(session.resets_at, None);
        assert_eq!(quotas[1].window_minutes, 10_080);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_first_request_after_a_reset_opens_the_next_window() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-reopen-{}", std::process::id()));
        let adapter = expired_session_adapter(&dir);
        // Twenty minutes after the old window ended.
        write_transcript(&dir, "s1.jsonl", &["2026-07-30T00:10:00.000Z"]);

        let quotas = adapter.quotas_at(instant("2026-07-30T01:00:00Z"));
        let session = &quotas[0];
        assert!(is_session_window(session));
        // Five hours from that first request, not from the old reset and not
        // from now.
        assert_eq!(session.resets_at, Some(instant("2026-07-30T05:10:00Z")));
        // A rolling window opens empty, and that was true when it opened.
        assert_eq!(session.used_percent_tenths, 0);
        assert_eq!(session.observed_at, instant("2026-07-30T00:10:00Z"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn windows_tile_forward_across_a_long_days_work() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-tile-{}", std::process::id()));
        let adapter = expired_session_adapter(&dir);
        // A window opens at 00:10 and ends at 05:10; the 05:30 request opens
        // the next one. The 02:00 message falls inside the first and starts
        // nothing.
        write_transcript(
            &dir,
            "s1.jsonl",
            &[
                "2026-07-30T00:10:00.000Z",
                "2026-07-30T02:00:00.000Z",
                "2026-07-30T05:30:00.000Z",
            ],
        );

        let quotas = adapter.quotas_at(instant("2026-07-30T06:00:00Z"));
        assert_eq!(quotas[0].resets_at, Some(instant("2026-07-30T10:30:00Z")));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_window_that_ran_out_while_idle_reports_no_window_at_all() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-idled-{}", std::process::id()));
        let adapter = expired_session_adapter(&dir);
        // One request, then the machine went quiet for hours. The window it
        // opened has itself expired, and nothing opened another.
        write_transcript(&dir, "s1.jsonl", &["2026-07-30T00:10:00.000Z"]);

        let quotas = adapter.quotas_at(instant("2026-07-30T08:00:00Z"));
        assert!(is_session_window(&quotas[0]));
        assert_eq!(quotas[0].resets_at, None);
        assert_eq!(quotas[0].used_percent_tenths, 0);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_live_cached_session_is_never_second_guessed() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-live-{}", std::process::id()));
        let adapter = expired_session_adapter(&dir);
        write_transcript(&dir, "s1.jsonl", &["2026-07-29T22:00:00.000Z"]);

        // Before the cached reset: the source's own percentage stands, and no
        // derivation happens.
        let quotas = adapter.quotas_at(instant("2026-07-29T23:00:00Z"));
        assert_eq!(quotas.len(), 2);
        assert_eq!(quotas[0].used_percent_tenths, 510);
        assert_eq!(
            quotas[0].resets_at,
            Some(instant("2026-07-29T23:50:00.308400Z"))
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_derived_window_does_not_claim_the_source_just_spoke() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-fresh-{}", std::process::id()));
        let adapter = expired_session_adapter(&dir);

        // Hours after the cached session window ended, with nothing since.
        let quotas = adapter.quotas_at(instant("2026-07-30T08:00:00Z"));
        // The derived row inherits the cache's own stamp. `read_delta` reports
        // the newest observation as the source's freshness, so stamping this
        // with the current time would have the interface claim Claude Code had
        // just spoken while its percentages are hours old.
        let newest = quotas.iter().map(|quota| quota.observed_at).max().unwrap();
        assert_eq!(newest, instant("2026-07-29T21:08:09.614Z"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_timestamp_is_read_without_parsing_the_line_around_it() {
        assert_eq!(
            line_timestamp(r#"{"type":"assistant","timestamp":"2026-07-30T00:10:00.000Z","x":1}"#),
            Some(instant("2026-07-30T00:10:00Z"))
        );
        assert_eq!(line_timestamp(r#"{"type":"summary"}"#), None);
        assert_eq!(line_timestamp(r#"{"timestamp":"not a time"}"#), None);
        assert_eq!(line_timestamp("not json at all"), None);
    }

    #[test]
    fn a_config_without_the_cache_falls_back_to_transcripts() {
        let dir =
            std::env::temp_dir().join(format!("tokens-claude-nocache-{}", std::process::id()));
        let adapter = adapter_with_config(&dir, r#"{"numStartups":1}"#);
        assert!(adapter.quotas().unwrap().is_empty());

        // A live 429 in a transcript still proves the window is exhausted.
        let line = serde_json::json!({
            "timestamp": "2999-01-01T00:00:00Z",
            "error": "rate_limit",
            "message": { "content": [{ "text": "You've hit your session limit · resets 11:59pm (America/Panama)" }] },
        });
        let project = dir.join("projects").join("-Users-dev-project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("session.jsonl"), format!("{line}\n")).unwrap();

        let quotas = adapter.quotas().unwrap();
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0].used_percent_tenths, 1_000);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_or_malformed_config_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("tokens-claude-bad-{}", std::process::id()));
        let adapter = adapter_with_config(&dir, "{ not json");
        assert!(adapter.quotas().unwrap().is_empty());
        fs::remove_dir_all(&dir).unwrap();

        let missing = ClaudeCodeAdapter::with_paths(
            PathBuf::from("/definitely/not/here"),
            PathBuf::from("/definitely/not/here.json"),
        );
        assert!(missing.quotas().unwrap().is_empty());
    }

    #[test]
    fn percentages_become_tenths_without_drifting() {
        assert_eq!(percent_to_tenths(0.0), 0);
        assert_eq!(percent_to_tenths(51.0), 510);
        assert_eq!(percent_to_tenths(85.575), 856);
        assert_eq!(percent_to_tenths(100.0), 1_000);
        // A server that over-reports still cannot exceed a full window, and a
        // nonsensical value floors at zero rather than wrapping.
        assert_eq!(percent_to_tenths(140.0), 1_000);
        assert_eq!(percent_to_tenths(-5.0), 0);
        assert_eq!(percent_to_tenths(f64::NAN), 0);
    }

    #[test]
    fn quota_scans_real_transcripts() {
        let root = std::env::temp_dir().join(format!("tokens-claude-quota-{}", std::process::id()));
        let project = root.join("projects").join("-Users-dev-project");
        fs::create_dir_all(&project).unwrap();
        // A reset far in the future of any test run keeps this live.
        let line = serde_json::json!({
            "timestamp": "2999-01-01T00:00:00Z",
            "error": "rate_limit",
            "message": { "content": [{ "text": "You've hit your weekly limit · resets 11:59pm (America/Panama)" }] },
        });
        fs::write(project.join("session-one.jsonl"), format!("{line}\n")).unwrap();

        let adapter = ClaudeCodeAdapter::with_root(root.clone());
        let quotas = adapter.quotas().unwrap();
        assert_eq!(quotas[0].window_minutes, 10_080);

        fs::remove_dir_all(&root).unwrap();

        let quiet = ClaudeCodeAdapter::with_root(PathBuf::from("/definitely/not/here"));
        assert!(quiet.quotas().unwrap().is_empty());
    }

    /// A scratch `.claude` tree, cleaned up by the caller.
    fn scratch(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("tokens-claude-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(PROJECTS_DIR).join("app")).unwrap();
        root
    }

    /// Run one delta, feeding back the checkpoints the previous one produced.
    fn tail(adapter: &ClaudeCodeAdapter, carried: &mut Vec<SourceCheckpoint>) -> SourceDelta {
        let request = DeltaRequest {
            mode: SyncMode::Incremental,
            checkpoints: carried.clone(),
            now: Utc::now(),
        };
        let delta = adapter.read_delta(&request).unwrap();
        for checkpoint in &delta.checkpoints {
            carried.retain(|kept| kept.source_key != checkpoint.source_key);
            carried.push(checkpoint.clone());
        }
        delta
    }

    fn assistant_line(id: &str, output: u64) -> String {
        format!(
            r#"{{"type":"assistant","uuid":"u-{id}","timestamp":"2026-07-29T10:00:00.000Z","cwd":"/x/app","sessionId":"s1","message":{{"id":"{id}","model":"claude-opus-5","usage":{{"input_tokens":100,"output_tokens":{output},"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}}}}"#
        )
    }

    #[test]
    fn streaming_snapshots_converge_on_the_final_counts() {
        let root = scratch("streaming");
        let path = root.join(PROJECTS_DIR).join("app").join("s1.jsonl");
        // Three snapshots of one in-flight message, as streaming writes them.
        fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                assistant_line("msg_01", 5),
                assistant_line("msg_01", 60),
                assistant_line("msg_01", 214),
            ),
        )
        .unwrap();

        let adapter = ClaudeCodeAdapter::with_root(root.clone());
        let delta = tail(&adapter, &mut Vec::new());

        // One draft, carrying the last snapshot: identity is the message id,
        // so the interim values never become separate records.
        assert_eq!(delta.drafts.len(), 1);
        assert_eq!(delta.drafts[0].source_event_id.as_deref(), Some("msg_01"));
        assert_eq!(delta.drafts[0].tokens.output, TokenField::exact(214));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_snapshot_arriving_in_a_later_chunk_keeps_the_same_identity() {
        let root = scratch("chunked");
        let path = root.join(PROJECTS_DIR).join("app").join("s1.jsonl");
        fs::write(&path, format!("{}\n", assistant_line("msg_01", 5))).unwrap();

        let adapter = ClaudeCodeAdapter::with_root(root.clone());
        let mut carried = Vec::new();
        let first = tail(&adapter, &mut carried);
        assert_eq!(first.drafts[0].tokens.output, TokenField::exact(5));

        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(file, "{}", assistant_line("msg_01", 214)).unwrap();
        drop(file);

        // The second chunk knows nothing of the first, but the id is the same,
        // so the repository updates the row rather than adding one.
        let second = tail(&adapter, &mut carried);
        assert_eq!(second.drafts.len(), 1);
        assert_eq!(second.drafts[0].source_event_id.as_deref(), Some("msg_01"));
        assert_eq!(second.drafts[0].tokens.output, TokenField::exact(214));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_new_nested_subagent_log_is_found_without_a_restart() {
        let root = scratch("subagent");
        let path = root.join(PROJECTS_DIR).join("app").join("s1.jsonl");
        fs::write(&path, format!("{}\n", assistant_line("msg_01", 5))).unwrap();

        let adapter = ClaudeCodeAdapter::with_root(root.clone());
        let mut carried = Vec::new();
        assert_eq!(tail(&adapter, &mut carried).drafts.len(), 1);

        // A Task tool subagent creates its directory mid-session; its usage
        // appears nowhere else, so the walk has to find it on the next pass.
        let nested = root
            .join(PROJECTS_DIR)
            .join("app")
            .join("s1")
            .join("subagents");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("agent-1.jsonl"),
            format!("{}\n", assistant_line("msg_02", 40)),
        )
        .unwrap();

        let second = tail(&adapter, &mut carried);
        assert_eq!(second.drafts.len(), 1);
        assert_eq!(second.drafts[0].source_event_id.as_deref(), Some("msg_02"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_partial_final_line_is_held_until_it_completes() {
        let root = scratch("partial");
        let path = root.join(PROJECTS_DIR).join("app").join("s1.jsonl");
        let whole = assistant_line("msg_01", 5);
        let torn = assistant_line("msg_02", 214);
        fs::write(&path, format!("{whole}\n{}", &torn[..torn.len() / 2])).unwrap();

        let adapter = ClaudeCodeAdapter::with_root(root.clone());
        let mut carried = Vec::new();
        let first = tail(&adapter, &mut carried);
        assert_eq!(first.drafts.len(), 1);
        assert!(first.failures.is_empty());

        fs::write(&path, format!("{whole}\n{torn}\n")).unwrap();
        let second = tail(&adapter, &mut carried);
        assert_eq!(second.drafts.len(), 1);
        assert_eq!(second.drafts[0].source_event_id.as_deref(), Some("msg_02"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn allowance_freshness_is_the_sources_own_stamp() {
        let root = scratch("freshness");
        fs::write(
            root.join(PROJECTS_DIR).join("app").join("s1.jsonl"),
            format!("{}\n", assistant_line("msg_01", 5)),
        )
        .unwrap();
        let config = root.join("claude.json");
        // fetchedAtMs is when Claude Code got the numbers, which is not when
        // this app read the file. Reporting the read time would make an hour-
        // old allowance look live.
        let fetched_at = Utc::now() - chrono::Duration::minutes(40);
        fs::write(
            &config,
            config_json(
                fetched_at.timestamp_millis(),
                &(Utc::now() + chrono::Duration::hours(2)).to_rfc3339(),
                &(Utc::now() + chrono::Duration::days(3)).to_rfc3339(),
            ),
        )
        .unwrap();

        let adapter = ClaudeCodeAdapter::with_paths(root.clone(), config);
        let delta = tail(&adapter, &mut Vec::new());

        assert!(!delta.quotas.is_empty());
        let observed = delta.source_observed_at.unwrap();
        assert!((observed - fetched_at).num_seconds().abs() < 2);

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_watched_roots_cover_an_atomically_replaced_config() {
        let adapter = ClaudeCodeAdapter::with_paths(
            PathBuf::from("/home/.claude"),
            PathBuf::from("/home/.claude.json"),
        );
        let roots = adapter.watch_roots();

        // The transcripts are a tree: a Task subagent creates a nested
        // directory mid-session and its usage appears nowhere else.
        assert!(roots.contains(&WatchRoot::tree(PathBuf::from("/home/.claude/projects"))));
        // The config file's parent, not the file: an atomic write replaces the
        // inode and a watch on the old one would go quiet forever. Shallow,
        // because that parent is the user's home directory and following it
        // recursively subscribes to every write on the machine.
        assert!(roots.contains(&WatchRoot::shallow(PathBuf::from("/home"))));
        assert!(!roots
            .iter()
            .any(|root| root.recursive && root.path == std::path::Path::new("/home")));
        assert!(!roots
            .iter()
            .any(|root| root.path == std::path::Path::new("/home/.claude.json")));
    }
}
