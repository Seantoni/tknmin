//! Cursor adapter.
//!
//! Cursor does not keep a Codex-style transcript. Usage lives in the global
//! VS Code state database:
//!
//! ```text
//! ~/Library/Application Support/Cursor/User/globalStorage/state.vscdb
//!   └── cursorDiskKV
//!         bubbleId:<conversationId>:<bubbleId>  → JSON bubble
//! ```
//!
//! Two eras of data have to coexist:
//!
//! - Through roughly January 2026, some assistant bubbles carry real
//!   `tokenCount` values and a unique `usageUuid`. Those are imported as
//!   token-bearing records.
//! - After that, `tokenCount` is almost always `{0,0}` (Cursor's own client
//!   backfill is best-effort). User bubbles still carry `modelInfo` and
//!   `requestId`, so recent activity still becomes events — with every token
//!   field left unknown rather than invented.
//!
//! The unit of import is therefore mixed on purpose: a non-zero assistant
//! bubble is one record; a user turn whose following assistants have no
//! tokens is one record. That keeps historical token totals without
//! double-counting, and keeps recent model/event activity visible.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::domain::{
    CostCalculationStatus, CostDraft, Money, ReplaceScope, SourceApp, SourceCheckpoint,
    TokenCounts, TokenField, UsageQuota, UsageRecordDraft,
};

use super::{
    AdapterError, DeltaRequest, DiscoveredSource, RawSourceInput, SourceAdapter, SourceDelta,
    SourceFormat, SyncMode,
};

const SOURCE_REF: &str = "global-bubbles";
const SOURCE_LABEL: &str = "Cursor global state";
const DASHBOARD_SOURCE_REF: &str = "dashboard-usage-events";
const DASHBOARD_SOURCE_LABEL: &str = "Cursor dashboard usage";
const USAGE_ENDPOINT: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const EVENTS_ENDPOINT: &str = "https://cursor.com/api/dashboard/get-filtered-usage-events";
const KEYRING_SERVICE: &str = "com.josep.tokens";
const KEYRING_USER: &str = "cursor-dashboard-session";
const DASHBOARD_WINDOW_DAYS: i64 = 30;
const EVENTS_PAGE_SIZE: u64 = 500;
const BILLING_MONTH_MINUTES: u32 = 30 * 24 * 60;

/// `None` means Keychain has not been read yet; `Some(None)` means it was read
/// and no credential exists. The mutex stays locked through the first read so
/// concurrent startup tasks cannot open duplicate authorization dialogs.
static DASHBOARD_TOKEN_CACHE: OnceLock<Mutex<Option<Option<String>>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorConnectionStatus {
    pub connected: bool,
}

#[derive(Debug, Clone)]
pub struct CursorAdapter {
    /// Path to `state.vscdb`. Overridable so tests never touch the live DB.
    db_path: PathBuf,
    usage_endpoint: String,
    /// Disabled by fixture constructors so tests never touch Keychain/network.
    allow_dashboard: bool,
}

fn dashboard_entry() -> Result<keyring::Entry, AdapterError> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
        .map_err(|error| dashboard_error(format!("could not open macOS Keychain: {error}")))
}

fn dashboard_token() -> Result<Option<String>, AdapterError> {
    let mut cache = DASHBOARD_TOKEN_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| dashboard_error("dashboard credential cache is unavailable".to_string()))?;
    if let Some(token) = cache.as_ref() {
        return Ok(token.clone());
    }

    let token = match dashboard_entry()?.get_password() {
        Ok(token) if !token.trim().is_empty() => Ok(Some(token)),
        Ok(_) | Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(dashboard_error(format!("could not read macOS Keychain: {error}"))),
    }?;
    *cache = Some(token.clone());
    Ok(token)
}

fn cache_dashboard_token(token: Option<String>) -> Result<(), AdapterError> {
    let mut cache = DASHBOARD_TOKEN_CACHE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| dashboard_error("dashboard credential cache is unavailable".to_string()))?;
    *cache = Some(token);
    Ok(())
}

pub fn cursor_connection_status() -> Result<CursorConnectionStatus, AdapterError> {
    Ok(CursorConnectionStatus { connected: dashboard_token()?.is_some() })
}

pub fn connect_cursor_dashboard(session_token: &str) -> Result<CursorConnectionStatus, AdapterError> {
    let token = session_token.trim();
    if token.is_empty() || token.contains('\r') || token.contains('\n') {
        return Err(dashboard_error("the Cursor dashboard session token is invalid".to_string()));
    }
    // Validate before storing. The one-event request proves both authentication
    // and endpoint access without downloading the account history twice.
    fetch_usage_events_page(token, None, None, 1, 1)?;
    dashboard_entry()?
        .set_password(token)
        .map_err(|error| dashboard_error(format!("could not save to macOS Keychain: {error}")))?;
    cache_dashboard_token(Some(token.to_string()))?;
    Ok(CursorConnectionStatus { connected: true })
}

pub fn disconnect_cursor_dashboard() -> Result<CursorConnectionStatus, AdapterError> {
    match dashboard_entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {
            cache_dashboard_token(None)?;
            Ok(CursorConnectionStatus { connected: false })
        }
        Err(error) => Err(dashboard_error(format!("could not update macOS Keychain: {error}"))),
    }
}

impl CursorAdapter {
    pub const ID: &'static str = "cursor";
    pub const VERSION: &'static str = "0.3.0";

    pub fn new() -> Self {
        Self {
            db_path: default_state_db(),
            usage_endpoint: USAGE_ENDPOINT.to_string(),
            allow_dashboard: true,
        }
    }

    pub fn with_db(db_path: PathBuf) -> Self {
        Self {
            db_path,
            usage_endpoint: USAGE_ENDPOINT.to_string(),
            allow_dashboard: false,
        }
    }

    fn open_db(&self) -> Result<Connection, AdapterError> {
        open_readonly(&self.db_path)
    }

    fn access_token(&self) -> Result<String, AdapterError> {
        let connection = self.open_db()?;
        connection
            .query_row(
                "SELECT value FROM ItemTable WHERE key='cursorAuth/accessToken' LIMIT 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|token| token.trim_matches('"').to_string())
            .map_err(|_| AdapterError::Unreadable {
                adapter: Self::ID,
                reason: "Cursor is not signed in (no local access token)".to_string(),
            })
    }

    /// Page through one window of billed events.
    ///
    /// The window is an argument rather than a constant so the fast path can
    /// ask for a few hours while reconciliation asks for the full rolling
    /// month, through one implementation.
    fn fetch_dashboard_events(
        &self,
        from: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<String, AdapterError> {
        let token = dashboard_token()?.ok_or_else(|| {
            dashboard_error("Cursor dashboard is not connected".to_string())
        })?;
        let start_ms = from.timestamp_millis().to_string();
        let end_ms = until.timestamp_millis().to_string();
        let client = http_client(Duration::from_secs(20))?;
        let mut page = 1;
        let mut events = Vec::new();

        loop {
            let response = fetch_usage_events_page_with(
                &client,
                &token,
                Some(&start_ms),
                Some(&end_ms),
                page,
                EVENTS_PAGE_SIZE,
            )?;
            let total = response
                .get("totalUsageEventsCount")
                .and_then(json_u64)
                .unwrap_or_default();
            let page_events = response
                .get("usageEventsDisplay")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    dashboard_error(
                        "Cursor dashboard response did not contain usage events".to_string(),
                    )
                })?;
            let fetched = page_events.len();
            events.extend(page_events.iter().cloned());
            if fetched == 0 || events.len() as u64 >= total {
                break;
            }
            page += 1;
            if page > 200 {
                return Err(dashboard_error(
                    "Cursor dashboard returned too many result pages".to_string(),
                ));
            }
        }

        serde_json::to_string(&serde_json::json!({ "usageEventsDisplay": events }))
            .map_err(|error| dashboard_error(format!("could not encode Cursor usage: {error}")))
    }

    /// Cursor's local bubbles no longer contain dependable token counts, and
    /// unlike Claude it does not cache account utilization on disk. The
    /// installed client does keep its access token in `state.vscdb`, so this
    /// calls the same Cursor-hosted current-period endpoint used by Cursor's
    /// billing UI. The credential stays in memory, is sent only to
    /// `api2.cursor.sh`, and is never logged or persisted by this app.
    ///
    /// There is no adapter-side cache of the answer. A failed fetch simply
    /// fails, and the repository keeps the last committed snapshot — one
    /// last-known-good store rather than two that can disagree.
    fn fetch_current_period_quotas(&self) -> Result<Vec<UsageQuota>, AdapterError> {
        let token = self.access_token()?;
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(12))
            .build()
            .map_err(|error| quota_error(format!("could not create HTTPS client: {error}")))?;

        let response = client
            .post(&self.usage_endpoint)
            .bearer_auth(token)
            .header("Content-Type", "application/json")
            .header("Connect-Protocol-Version", "1")
            .json(&serde_json::json!({}))
            .send()
            .map_err(|error| quota_error(format!("Cursor usage request failed: {error}")))?;

        if !response.status().is_success() {
            return Err(quota_error(format!(
                "Cursor usage request returned HTTP {}",
                response.status()
            )));
        }

        let value: serde_json::Value = response
            .json()
            .map_err(|error| quota_error(format!("Cursor usage response was not JSON: {error}")))?;
        parse_usage_response(&value, Utc::now()).filter(|quotas| !quotas.is_empty()).ok_or_else(|| {
            quota_error("Cursor usage response did not contain current plan usage".to_string())
        })
    }

}

impl Default for CursorAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// One HTTPS client, built once per request sequence rather than once per
/// page: a 30-day backfill is dozens of pages, and each fresh client is a
/// fresh TLS handshake to the same host.
fn http_client(timeout: Duration) -> Result<reqwest::blocking::Client, AdapterError> {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| dashboard_error(format!("could not create HTTPS client: {error}")))
}

fn fetch_usage_events_page(
    token: &str,
    start_date: Option<&str>,
    end_date: Option<&str>,
    page: u64,
    page_size: u64,
) -> Result<serde_json::Value, AdapterError> {
    let client = http_client(Duration::from_secs(20))?;
    fetch_usage_events_page_with(&client, token, start_date, end_date, page, page_size)
}

fn fetch_usage_events_page_with(
    client: &reqwest::blocking::Client,
    token: &str,
    start_date: Option<&str>,
    end_date: Option<&str>,
    page: u64,
    page_size: u64,
) -> Result<serde_json::Value, AdapterError> {
    let mut body = serde_json::json!({ "page": page, "pageSize": page_size });
    if let Some(start) = start_date {
        body["startDate"] = serde_json::Value::String(start.to_string());
    }
    if let Some(end) = end_date {
        body["endDate"] = serde_json::Value::String(end.to_string());
    }

    let response = client
        .post(EVENTS_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Origin", "https://cursor.com")
        .header("Cookie", format!("WorkosCursorSessionToken={token}"))
        .json(&body)
        .send()
        .map_err(|error| {
            // A transport failure is "cannot reach", not "is broken": the
            // interface should say offline and keep the last-good numbers.
            if error.is_timeout() || error.is_connect() {
                AdapterError::Offline {
                    adapter: CursorAdapter::ID,
                    reason: "Cursor could not be reached".to_string(),
                }
            } else {
                dashboard_error("the Cursor dashboard request failed".to_string())
            }
        })?;

    if !response.status().is_success() {
        let status = response.status();
        if status.as_u16() == 429 {
            // Honour the provider's own answer when it gives one; the
            // coordinator's backoff decides the rest.
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs);
            return Err(AdapterError::RateLimited {
                adapter: CursorAdapter::ID,
                retry_after,
            });
        }
        if status.is_server_error() {
            return Err(AdapterError::Offline {
                adapter: CursorAdapter::ID,
                reason: format!("Cursor returned HTTP {status}"),
            });
        }
        let reason = if status.as_u16() == 401 {
            "Cursor rejected the session token; copy a fresh WorkosCursorSessionToken"
                .to_string()
        } else {
            format!("Cursor dashboard request returned HTTP {status}")
        };
        return Err(dashboard_error(reason));
    }

    response
        .json()
        .map_err(|error| dashboard_error(format!("Cursor dashboard response was not JSON: {error}")))
}

/// What the dashboard sync has already pulled.
///
/// Cursor's usage events carry no immutable server identifier, and a cost can
/// be corrected after the fact. So the fast path fetches from a deliberate
/// overlap *before* the watermark and hands the coordinator a replace scope
/// for that slice: whatever the server now says about that window is what the
/// store ends up holding, and a corrected cost cannot coexist with the
/// obsolete one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DashboardCheckpoint {
    /// The newest event timestamp the server has published, in milliseconds.
    #[serde(default)]
    pub watermark_ms: Option<i64>,
    /// Whether the last local activity has been reflected in server data yet.
    #[serde(default)]
    pub awaiting_upstream: bool,
}

/// What the local bubble read has already seen.
///
/// `state.vscdb` has no row-level change stamp to key on, so the local path
/// re-reads the database and replaces its own window transactionally. Size and
/// modification time gate that: a database Cursor has not touched cannot have
/// new bubbles, so a burst of watcher events costs one metadata check.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalCheckpoint {
    #[serde(default)]
    pub db_size: u64,
    #[serde(default)]
    pub wal_size: u64,
    #[serde(default)]
    pub modified_ms: Option<i64>,
}

/// Checkpoint keys. Two, because local and dashboard are different datasets
/// over the same source and must never resume from each other's position.
const LOCAL_CHECKPOINT: &str = "local-bubbles";
const DASHBOARD_CHECKPOINT: &str = "dashboard-window";

/// How far before the watermark a fast delta re-reads. Cursor publishes billed
/// events out of order and corrects them, so a window that only looked forward
/// would miss both.
const DELTA_OVERLAP: chrono::Duration = chrono::Duration::hours(6);

impl SourceAdapter for CursorAdapter {
    fn id(&self) -> &'static str {
        Self::ID
    }

    fn version(&self) -> &'static str {
        Self::VERSION
    }

    fn source_app(&self) -> SourceApp {
        SourceApp::Cursor
    }

    /// The global-storage directory rather than `state.vscdb` itself: SQLite
    /// writes through `-wal` and `-shm` siblings and replaces files on
    /// checkpoint, so only the directory sees every form the activity takes.
    fn watch_roots(&self) -> Vec<PathBuf> {
        self.db_path
            .parent()
            .map(|parent| vec![parent.to_path_buf()])
            .unwrap_or_default()
    }

    fn needs_network(&self) -> bool {
        true
    }

    fn read_delta(&self, request: &DeltaRequest) -> Result<SourceDelta, AdapterError> {
        if request.mode == SyncMode::QuotaOnly {
            let quotas = self.fetch_current_period_quotas()?;
            let source_observed_at = quotas.iter().map(|quota| quota.observed_at).max();
            return Ok(SourceDelta {
                replace_quotas: !quotas.is_empty(),
                quotas,
                source_observed_at,
                ..SourceDelta::default()
            });
        }

        if self.allow_dashboard && dashboard_token()?.is_some() {
            self.read_dashboard_delta(request)
        } else {
            self.read_local_delta(request)
        }
    }
}

impl CursorAdapter {
    /// The authoritative path: billed events from Cursor's own dashboard.
    ///
    /// Two shapes of work, both owned by the coordinator's mode. A fast delta
    /// covers the overlap window after detected activity; a reconciliation
    /// re-reads the whole rolling 30 days so corrections, deletions, and late
    /// publication converge. Neither downloads 30 days on the fast path.
    fn read_dashboard_delta(&self, request: &DeltaRequest) -> Result<SourceDelta, AdapterError> {
        let stored: DashboardCheckpoint = request.resume(DASHBOARD_CHECKPOINT).unwrap_or_default();
        let full_window = request.now - chrono::Duration::days(DASHBOARD_WINDOW_DAYS);

        let from = match (request.mode, stored.watermark_ms.and_then(instant_ms)) {
            (SyncMode::Incremental, Some(watermark)) => (watermark - DELTA_OVERLAP).max(full_window),
            _ => full_window,
        };

        let events = self.fetch_dashboard_events(from, request.now)?;
        let input = RawSourceInput {
            source_ref: Some(DASHBOARD_SOURCE_REF.to_string()),
            format: SourceFormat::Json,
            content: events,
        };
        let drafts = parse_dashboard_events(self, &input)?;

        let newest = drafts
            .iter()
            .filter_map(|draft| draft.raw_timestamp.as_deref())
            .filter_map(parse_event_instant)
            .max();

        // Local activity newer than anything the server has published means
        // billing has not caught up. Saying so is the honest answer; showing
        // the gap as zero usage would not be.
        let awaiting_upstream = self
            .local_activity_at()
            .is_some_and(|activity| newest.is_none_or(|published| activity > published));

        Ok(SourceDelta {
            drafts,
            replace_scopes: vec![ReplaceScope {
                source_app: SourceApp::Cursor,
                adapter_id: Self::ID.to_string(),
                from,
                until: request.now,
            }],
            checkpoints: vec![SourceCheckpoint {
                adapter_id: Self::ID.to_string(),
                source_key: DASHBOARD_CHECKPOINT.to_string(),
                payload: serde_json::to_value(DashboardCheckpoint {
                    watermark_ms: newest
                        .map(|at| at.timestamp_millis())
                        .or(stored.watermark_ms),
                    awaiting_upstream,
                })
                .unwrap_or_default(),
            }],
            source_observed_at: newest,
            awaiting_upstream,
            ..SourceDelta::default()
        })
    }

    /// The fallback path: local bubbles, when no dashboard session is stored.
    ///
    /// Bubbles are mutable — Cursor backfills token counts into them after the
    /// fact — and carry no change stamp, so this re-reads and replaces its own
    /// window rather than trying to detect changed rows. The metadata gate is
    /// what keeps that affordable under a write burst.
    fn read_local_delta(&self, request: &DeltaRequest) -> Result<SourceDelta, AdapterError> {
        let stored: LocalCheckpoint = request.resume(LOCAL_CHECKPOINT).unwrap_or_default();
        let current = self.local_state();

        if request.mode == SyncMode::Incremental
            && current.db_size == stored.db_size
            && current.wal_size == stored.wal_size
            && current.modified_ms == stored.modified_ms
            && stored.modified_ms.is_some()
        {
            return Ok(SourceDelta::default());
        }

        let connection = self.open_db()?;
        let content =
            extract_bubbles_jsonl(&connection).map_err(|error| AdapterError::Unreadable {
                adapter: Self::ID,
                reason: error,
            })?;
        let drafts = self.parse(&RawSourceInput {
            source_ref: Some(SOURCE_REF.to_string()),
            format: SourceFormat::Jsonl,
            content,
        })?;

        let newest = drafts
            .iter()
            .filter_map(|draft| draft.raw_timestamp.as_deref())
            .filter_map(parse_event_instant)
            .max();

        // Bubbles can be rewritten, so the batch replaces the slice it covers:
        // an updated bubble repairs its record instead of leaving the obsolete
        // unknown-token version behind.
        let from = drafts
            .iter()
            .filter_map(|draft| draft.raw_timestamp.as_deref())
            .filter_map(parse_event_instant)
            .min();

        Ok(SourceDelta {
            replace_scopes: from
                .map(|from| ReplaceScope {
                    source_app: SourceApp::Cursor,
                    adapter_id: Self::ID.to_string(),
                    from,
                    until: request.now,
                })
                .into_iter()
                .collect(),
            drafts,
            checkpoints: vec![SourceCheckpoint {
                adapter_id: Self::ID.to_string(),
                source_key: LOCAL_CHECKPOINT.to_string(),
                payload: serde_json::to_value(&current).unwrap_or_default(),
            }],
            source_observed_at: newest,
            ..SourceDelta::default()
        })
    }

    /// Size and modification time of the local database and its write-ahead
    /// log — the cheapest possible "did anything happen".
    fn local_state(&self) -> LocalCheckpoint {
        let size = |path: PathBuf| fs::metadata(&path).map(|meta| meta.len()).unwrap_or_default();
        let modified_ms = fs::metadata(&self.db_path)
            .and_then(|meta| meta.modified())
            .ok()
            .map(|at| DateTime::<Utc>::from(at).timestamp_millis());

        LocalCheckpoint {
            db_size: size(self.db_path.clone()),
            wal_size: size(wal_path(&self.db_path)),
            modified_ms,
        }
    }

    /// When Cursor last wrote locally, which is when the user last used it —
    /// not when the resulting charge was published.
    fn local_activity_at(&self) -> Option<DateTime<Utc>> {
        let newest = |path: PathBuf| {
            fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()
                .map(DateTime::<Utc>::from)
        };
        [newest(self.db_path.clone()), newest(wal_path(&self.db_path))]
            .into_iter()
            .flatten()
            .max()
    }

    /// Locate this adapter's sources. Kept for tests and diagnostics; the
    /// running application reaches sources through [`SourceAdapter::read_delta`].
    pub fn discover(&self) -> Result<Vec<DiscoveredSource>, AdapterError> {
        if self.allow_dashboard && dashboard_token()?.is_some() {
            return Ok(vec![DiscoveredSource {
                source_ref: DASHBOARD_SOURCE_REF.to_string(),
                label: DASHBOARD_SOURCE_LABEL.to_string(),
                format: SourceFormat::Json,
            }]);
        }
        if !self.db_path.is_file() {
            return Err(AdapterError::Discovery {
                adapter: Self::ID,
                reason: format!("no state database at {}", self.db_path.display()),
            });
        }
        // Confirm the table exists before advertising the source as readable.
        let connection = self.open_db()?;
        let has_table: bool = connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='cursorDiskKV' LIMIT 1",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !has_table {
            return Err(AdapterError::Discovery {
                adapter: Self::ID,
                reason: "state.vscdb has no cursorDiskKV table".to_string(),
            });
        }

        Ok(vec![DiscoveredSource {
            source_ref: SOURCE_REF.to_string(),
            label: SOURCE_LABEL.to_string(),
            format: SourceFormat::Sqlite,
        }])
    }

    pub fn read(&self, source: &DiscoveredSource) -> Result<RawSourceInput, AdapterError> {
        if source.source_ref == DASHBOARD_SOURCE_REF {
            let until = Utc::now();
            return Ok(RawSourceInput::from_source(
                source,
                self.fetch_dashboard_events(
                    until - chrono::Duration::days(DASHBOARD_WINDOW_DAYS),
                    until,
                )?,
            ));
        }
        if source.source_ref != SOURCE_REF {
            return Err(AdapterError::Unreadable {
                adapter: Self::ID,
                reason: "unknown source ref".to_string(),
            });
        }

        let connection = self.open_db()?;
        let content = extract_bubbles_jsonl(&connection).map_err(|error| AdapterError::Unreadable {
            adapter: Self::ID,
            reason: error,
        })?;

        Ok(RawSourceInput {
            source_ref: Some(source.source_ref.clone()),
            format: SourceFormat::Jsonl,
            content,
        })
    }

    pub fn parse(&self, input: &RawSourceInput) -> Result<Vec<UsageRecordDraft>, AdapterError> {
        if input.source_ref.as_deref() == Some(DASHBOARD_SOURCE_REF) {
            return parse_dashboard_events(self, input);
        }
        let mut by_conversation: HashMap<String, Vec<CompactBubble>> = HashMap::new();

        for line in input.content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let bubble: CompactBubble = match serde_json::from_str(line) {
                Ok(bubble) => bubble,
                Err(_) => continue,
            };
            by_conversation.entry(bubble.conversation_id.clone()).or_default().push(bubble);
        }

        let mut drafts = Vec::new();
        for mut bubbles in by_conversation.into_values() {
            bubbles.sort_by(|left, right| left.created_at.cmp(&right.created_at));
            drafts.extend(drafts_for_conversation(self, input.source_ref.as_deref(), &bubbles));
        }

        // Stable order for reproducible imports.
        drafts.sort_by(|left, right| {
            left.raw_timestamp
                .cmp(&right.raw_timestamp)
                .then(left.source_event_id.cmp(&right.source_event_id))
        });
        Ok(drafts)
    }
}

fn quota_error(reason: String) -> AdapterError {
    AdapterError::Unreadable { adapter: CursorAdapter::ID, reason }
}

fn dashboard_error(reason: String) -> AdapterError {
    AdapterError::Unreadable { adapter: CursorAdapter::ID, reason }
}

fn parse_dashboard_events(
    adapter: &CursorAdapter,
    input: &RawSourceInput,
) -> Result<Vec<UsageRecordDraft>, AdapterError> {
    let root: serde_json::Value = serde_json::from_str(&input.content).map_err(|error| {
        AdapterError::Parse {
            adapter: CursorAdapter::ID,
            entry: 0,
            reason: format!("dashboard response was invalid: {error}"),
        }
    })?;
    let events = root
        .get("usageEventsDisplay")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| AdapterError::Parse {
            adapter: CursorAdapter::ID,
            entry: 0,
            reason: "dashboard response had no usageEventsDisplay array".to_string(),
        })?;

    let mut keyed: Vec<_> = events
        .iter()
        .map(|event| (dashboard_identity(event), event))
        .collect();
    keyed.sort_by(|(left_key, left), (right_key, right)| {
        event_timestamp(left)
            .cmp(&event_timestamp(right))
            .then(left_key.cmp(right_key))
    });

    let mut occurrences: HashMap<String, usize> = HashMap::new();
    let mut drafts = Vec::with_capacity(keyed.len());
    for (identity, event) in keyed {
        let occurrence = occurrences.entry(identity.clone()).or_default();
        let source_event_id = format!("dashboard-{identity}-{occurrence}");
        *occurrence += 1;

        let token_usage = event.get("tokenUsage");
        let input_tokens = token_usage.and_then(|usage| usage.get("inputTokens")).and_then(json_u64);
        let cache_write =
            token_usage.and_then(|usage| usage.get("cacheWriteTokens")).and_then(json_u64);
        let output_tokens =
            token_usage.and_then(|usage| usage.get("outputTokens")).and_then(json_u64);
        let cache_read =
            token_usage.and_then(|usage| usage.get("cacheReadTokens")).and_then(json_u64);
        let input_field = match (input_tokens, cache_write) {
            (None, None) => TokenField::unknown(),
            (input, write) => {
                TokenField::exact(input.unwrap_or_default().saturating_add(write.unwrap_or_default()))
            }
        };

        let mut draft =
            UsageRecordDraft::new(SourceApp::Cursor, adapter.provenance(input.source_ref.as_deref()))
                .with_source_event_id(source_event_id)
                .with_tokens(TokenCounts {
                    input: input_field,
                    output: output_tokens.map_or_else(TokenField::unknown, TokenField::exact),
                    cached_input: cache_read.map_or_else(TokenField::unknown, TokenField::exact),
                    reasoning: TokenField::unknown(),
                });
        if let Some(timestamp) = event_timestamp(event) {
            draft.raw_timestamp = Some(timestamp);
        }
        if let Some(model) = event.get("model").and_then(serde_json::Value::as_str) {
            draft = draft.with_model(provider_for_model(model), model);
        }
        let charged_cents = event
            .get("chargedCents")
            .and_then(number)
            .or_else(|| token_usage?.get("totalCents").and_then(number));
        if let Some(cost) = charged_cents.and_then(money_from_cents) {
            draft = draft.with_cost(CostDraft {
                amount: Some(cost),
                status: CostCalculationStatus::ReportedBySource,
                pricing_version: None,
            });
        }
        drafts.push(draft);
    }
    Ok(drafts)
}

/// What identifies one billed event across corrections.
///
/// Cursor's response has carried a server identifier under several names over
/// time; when one is present it is authoritative and nothing else matters.
/// When none is, identity falls back to the fields a correction is *not*
/// allowed to move — when it happened, which model ran, and whose request it
/// was — and deliberately excludes tokens and cost, which are exactly what a
/// correction changes. Getting that wrong is what makes a corrected charge
/// appear as a second charge.
///
/// The fallback cannot be perfect: two indistinguishable events in the same
/// millisecond are told apart only by their order, which is why the
/// coordinator also replaces the window transactionally.
fn dashboard_identity(event: &serde_json::Value) -> String {
    for field in ["id", "eventId", "usageEventId", "requestId"] {
        if let Some(id) = event.get(field).and_then(serde_json::Value::as_str) {
            if !id.trim().is_empty() {
                return format!("id:{id}");
            }
        }
    }

    let part = |field: &str| {
        event
            .get(field)
            .map(|value| match value.as_str() {
                Some(text) => text.to_string(),
                None => value.to_string(),
            })
            .unwrap_or_default()
    };
    let invariant = [
        event_timestamp(event).unwrap_or_default(),
        part("model"),
        part("kind"),
        part("kindLabel"),
        part("owningUser"),
        part("userEmail"),
    ]
    .join("\u{1f}");
    format!("inv:{}", hex::encode(Sha256::digest(invariant.as_bytes())))
}

/// A source timestamp as an instant, accepting both the ISO strings and the
/// epoch milliseconds Cursor has used at different times.
fn parse_event_instant(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(raw) {
        return Some(parsed.to_utc());
    }
    raw.parse::<i64>().ok().and_then(instant_ms)
}

fn instant_ms(millis: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(millis)
}

/// SQLite's write-ahead log sits beside the database and is where a live
/// Cursor session's newest writes are before a checkpoint folds them in.
fn wal_path(db_path: &Path) -> PathBuf {
    let mut name = db_path.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

fn event_timestamp(event: &serde_json::Value) -> Option<String> {
    let value = event.get("timestamp")?;
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|number| number.to_string()))
}

fn json_u64(value: &serde_json::Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn money_from_cents(cents: f64) -> Option<Money> {
    if !cents.is_finite() || cents < 0.0 || cents > i64::MAX as f64 / 100.0 {
        return None;
    }
    Money::new((cents * 100.0).round() as i64, "USD", 4).ok()
}

fn provider_for_model(model: &str) -> Option<&'static str> {
    let model = model.to_ascii_lowercase();
    if model.contains("claude") {
        Some("anthropic")
    } else if model.contains("gemini") {
        Some("google")
    } else if model.contains("gpt") || model.starts_with('o') {
        Some("openai")
    } else if model.contains("composer") || model.contains("grok") {
        Some("cursor")
    } else {
        None
    }
}

/// Normalize the current and known wrapped forms of Cursor's private Connect
/// response. Additive fields are ignored and numeric strings are accepted.
fn parse_usage_response(
    value: &serde_json::Value,
    observed_at: DateTime<Utc>,
) -> Option<Vec<UsageQuota>> {
    let root = if value.get("planUsage").is_some() {
        value
    } else {
        ["result", "usage", "data"]
            .into_iter()
            .filter_map(|key| value.get(key))
            .find(|candidate| candidate.get("planUsage").is_some())?
    };
    let plan = root.get("planUsage")?;

    let resets_at = instant(root.get("billingCycleEnd")?)?;
    let starts_at = root.get("billingCycleStart").and_then(instant);
    let window_minutes = starts_at
        .map(|start| (resets_at - start).num_minutes())
        .filter(|minutes| *minutes > 0)
        .and_then(|minutes| u32::try_from(minutes).ok())
        .unwrap_or(BILLING_MONTH_MINUTES);

    let pools = [
        ("Cursor Models", plan.get("autoPercentUsed").and_then(number)),
        ("Other Models", plan.get("apiPercentUsed").and_then(number)),
    ];
    let mut quotas: Vec<_> = pools
        .into_iter()
        .filter_map(|(label, percent)| {
            Some(UsageQuota {
                source_app: SourceApp::Cursor,
                label: Some(label.to_string()),
                window_minutes,
                used_percent_tenths: percent_to_tenths(percent?),
                resets_at: Some(resets_at),
                observed_at,
            })
        })
        .collect();

    // Older response shapes expose only a combined percentage. Preserve
    // support for those accounts without presenting it as either pool.
    if quotas.is_empty() {
        let total = plan.get("totalPercentUsed").and_then(number)?;
        quotas.push(UsageQuota {
            source_app: SourceApp::Cursor,
            label: Some("Total".to_string()),
            window_minutes,
            used_percent_tenths: percent_to_tenths(total),
            resets_at: Some(resets_at),
            observed_at,
        });
    }
    Some(quotas)
}

fn number(value: &serde_json::Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

fn instant(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    if let Some(milliseconds) = value.as_i64() {
        return DateTime::from_timestamp_millis(milliseconds);
    }
    let text = value.as_str()?;
    if let Ok(milliseconds) = text.parse::<i64>() {
        return DateTime::from_timestamp_millis(milliseconds);
    }
    DateTime::parse_from_rfc3339(text).ok().map(|date| date.to_utc())
}

fn percent_to_tenths(percent: f64) -> u16 {
    if !percent.is_finite() || percent <= 0.0 {
        return 0;
    }
    (percent * 10.0).round().min(1_000.0) as u16
}

/// One bubble stripped to the fields the import needs. Produced by `read`
/// so the multi-GB bubble payloads never leave the adapter's SQLite pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CompactBubble {
    conversation_id: String,
    bubble_id: String,
    /// 1 = user, 2 = assistant.
    #[serde(rename = "type")]
    bubble_type: i64,
    created_at: String,
    request_id: Option<String>,
    usage_uuid: Option<String>,
    model: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
}

impl CompactBubble {
    fn has_tokens(&self) -> bool {
        self.input_tokens > 0 || self.output_tokens > 0
    }

    fn is_user(&self) -> bool {
        self.bubble_type == 1
    }

    fn is_assistant(&self) -> bool {
        self.bubble_type == 2
    }
}

fn drafts_for_conversation(
    adapter: &CursorAdapter,
    source_ref: Option<&str>,
    bubbles: &[CompactBubble],
) -> Vec<UsageRecordDraft> {
    let mut drafts = Vec::new();
    let mut last_model: Option<String> = None;

    let mut index = 0;
    while index < bubbles.len() {
        let bubble = &bubbles[index];
        if let Some(model) = bubble.model.as_ref() {
            last_model = Some(model.clone());
        }

        // Token-bearing assistants outside a user-turn window (orphans, or
        // the walk's first sight of them) are imported on their own.
        if bubble.is_assistant() && bubble.has_tokens() {
            drafts.push(token_draft(adapter, source_ref, bubble, last_model.as_deref()));
            index += 1;
            continue;
        }

        if bubble.is_user() {
            let turn_end = bubbles[index + 1..]
                .iter()
                .position(|next| next.is_user())
                .map(|offset| index + 1 + offset)
                .unwrap_or(bubbles.len());

            // Emit every token-bearing assistant in this turn first, carrying
            // the user bubble's model forward.
            let mut covered_by_tokens = false;
            for following in &bubbles[index + 1..turn_end] {
                if let Some(model) = following.model.as_ref() {
                    last_model = Some(model.clone());
                }
                if following.is_assistant() && following.has_tokens() {
                    drafts.push(token_draft(
                        adapter,
                        source_ref,
                        following,
                        last_model.as_deref(),
                    ));
                    covered_by_tokens = true;
                }
            }

            // Recent-era turns have no token backfill: keep the user event so
            // model and request activity still show up.
            if !covered_by_tokens {
                drafts.push(event_draft(adapter, source_ref, bubble));
            }

            index = turn_end;
            continue;
        }

        index += 1;
    }

    drafts
}

fn token_draft(
    adapter: &CursorAdapter,
    source_ref: Option<&str>,
    bubble: &CompactBubble,
    model: Option<&str>,
) -> UsageRecordDraft {
    let event_id = bubble
        .usage_uuid
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| bubble.bubble_id.clone());

    let mut draft = UsageRecordDraft::new(SourceApp::Cursor, adapter.provenance(source_ref))
        .with_raw_timestamp(bubble.created_at.clone())
        .with_source_event_id(event_id)
        .with_tokens(TokenCounts {
            input: TokenField::exact(bubble.input_tokens),
            output: TokenField::exact(bubble.output_tokens),
            cached_input: TokenField::unknown(),
            reasoning: TokenField::unknown(),
        });
    draft.session_id = Some(bubble.conversation_id.clone());
    draft.model = model.or(bubble.model.as_deref()).map(str::to_string);
    draft
}

fn event_draft(
    adapter: &CursorAdapter,
    source_ref: Option<&str>,
    bubble: &CompactBubble,
) -> UsageRecordDraft {
    let event_id = bubble
        .request_id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| bubble.bubble_id.clone());

    let mut draft = UsageRecordDraft::new(SourceApp::Cursor, adapter.provenance(source_ref))
        .with_raw_timestamp(bubble.created_at.clone())
        .with_source_event_id(event_id)
        .with_tokens(TokenCounts::default());
    draft.session_id = Some(bubble.conversation_id.clone());
    draft.model = bubble.model.clone();
    draft
}

/// Pull every bubble row and rewrite it as a compact JSONL line.
fn extract_bubbles_jsonl(connection: &Connection) -> Result<String, String> {
    let mut statement = connection
        .prepare(
            "SELECT key, value FROM cursorDiskKV WHERE key LIKE 'bubbleId:%' AND length(key) >= 82",
        )
        .map_err(|error| error.to_string())?;

    let mut content = String::new();
    let rows = statement
        .query_map([], |row| {
            let key: String = row.get(0)?;
            let value: String = row.get(1)?;
            Ok((key, value))
        })
        .map_err(|error| error.to_string())?;

    for row in rows {
        let (key, value) = row.map_err(|error| error.to_string())?;
        let Some(bubble) = compact_from_row(&key, &value) else { continue };
        let line = serde_json::to_string(&bubble).map_err(|error| error.to_string())?;
        content.push_str(&line);
        content.push('\n');
    }
    Ok(content)
}

fn compact_from_row(key: &str, value: &str) -> Option<CompactBubble> {
    let (conversation_id, bubble_id) = parse_bubble_key(key)?;
    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;

    let bubble_type = parsed.get("type")?.as_i64()?;
    // Skip anything that is neither a user nor an assistant bubble.
    if bubble_type != 1 && bubble_type != 2 {
        return None;
    }

    let created_at = match parsed.get("createdAt") {
        Some(serde_json::Value::String(text)) if !text.is_empty() => text.clone(),
        Some(serde_json::Value::Number(number)) => {
            // Older rows sometimes store epoch milliseconds.
            let ms = number.as_i64()?;
            chrono::DateTime::from_timestamp_millis(ms)?.to_rfc3339()
        }
        _ => return None,
    };

    let token_count = parsed.get("tokenCount");
    let input_tokens = token_count
        .and_then(|count| count.get("inputTokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let output_tokens = token_count
        .and_then(|count| count.get("outputTokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    let model = parsed
        .get("modelInfo")
        .and_then(|info| info.get("modelName"))
        .and_then(serde_json::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(str::to_string);

    let request_id = parsed
        .get("requestId")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);

    let usage_uuid = parsed
        .get("usageUuid")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string);

    Some(CompactBubble {
        conversation_id,
        bubble_id,
        bubble_type,
        created_at,
        request_id,
        usage_uuid,
        model,
        input_tokens,
        output_tokens,
    })
}

/// `bubbleId:<36-char conv>:<36-char bubble>` — 82 characters with the prefix.
fn parse_bubble_key(key: &str) -> Option<(String, String)> {
    let rest = key.strip_prefix("bubbleId:")?;
    let (conversation_id, bubble_id) = rest.split_once(':')?;
    if conversation_id.len() != 36 || bubble_id.len() != 36 {
        return None;
    }
    Some((conversation_id.to_string(), bubble_id.to_string()))
}

fn open_readonly(path: &Path) -> Result<Connection, AdapterError> {
    // Prefer a plain read-only open so a recent WAL is visible. If Cursor has
    // the DB open and the image looks torn, fall back to immutable mode —
    // slightly staler, but never fights the writer.
    let uri = format!("file:{}?mode=ro", path.display());
    if let Ok(connection) =
        Connection::open_with_flags(&uri, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
    {
        // Touch the table once so a malformed open fails here, not mid-scan.
        if connection
            .query_row("SELECT 1 FROM cursorDiskKV LIMIT 1", [], |_| Ok(()))
            .is_ok()
            || connection
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='cursorDiskKV'",
                    [],
                    |_| Ok(()),
                )
                .is_ok()
        {
            return Ok(connection);
        }
    }

    let immutable = format!("file:{}?mode=ro&immutable=1", path.display());
    Connection::open_with_flags(
        &immutable,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| AdapterError::Unreadable {
        adapter: CursorAdapter::ID,
        reason: format!("{}: {error}", path.display()),
    })
}

fn default_state_db() -> PathBuf {
    home()
        .join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("."), PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::SourceFormat;
    use std::fs;

    const SAMPLE: &str = include_str!("../../fixtures/samples/cursor-bubbles.jsonl");

    fn parse_sample() -> Vec<UsageRecordDraft> {
        let adapter = CursorAdapter::new();
        let input = RawSourceInput {
            source_ref: Some(SOURCE_REF.to_string()),
            format: SourceFormat::Jsonl,
            content: SAMPLE.to_string(),
        };
        adapter.parse(&input).unwrap()
    }

    #[test]
    fn imports_token_bearing_assistants_and_recent_user_turns() {
        let drafts = parse_sample();
        // Sample layout:
        // - conversation A: one user turn followed by a token-bearing assistant
        //   → only the assistant (tokens already cover the turn)
        // - conversation A: later user turn with zero-token assistants
        //   → the user turn (recent-era event)
        // - conversation B: orphan token-bearing assistant
        //   → the assistant
        assert_eq!(drafts.len(), 3);

        let tokened: Vec<_> = drafts.iter().filter(|d| d.tokens.input.is_known()).collect();
        assert_eq!(tokened.len(), 2);
        assert_eq!(tokened[0].tokens.input, TokenField::exact(52_966));
        assert_eq!(tokened[0].tokens.output, TokenField::exact(14_911));
        assert_eq!(
            tokened[0].source_event_id.as_deref(),
            Some("9eb0dfd9-a6c1-4905-8eef-039adcb6636f")
        );
        assert_eq!(tokened[0].model.as_deref(), Some("gpt-5.2"));
        assert_eq!(
            tokened[0].session_id.as_deref(),
            Some("e09dc0ed-465b-4f71-9fb5-f3ba08e03288")
        );

        let events: Vec<_> = drafts.iter().filter(|d| !d.tokens.input.is_known()).collect();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].model.as_deref(), Some("grok-4.5"));
        assert_eq!(
            events[0].source_event_id.as_deref(),
            Some("a9dfc8a3-1600-4258-84f1-0092a2e87d02")
        );
        assert_eq!(events[0].tokens, TokenCounts::default());
    }

    #[test]
    fn event_ids_are_stable_across_parses() {
        let first: Vec<_> =
            parse_sample().into_iter().map(|d| d.source_event_id.unwrap()).collect();
        let second: Vec<_> =
            parse_sample().into_iter().map(|d| d.source_event_id.unwrap()).collect();
        assert_eq!(first, second);
    }

    #[test]
    fn dashboard_events_import_exact_tokens_model_and_cost() {
        let adapter = CursorAdapter::new();
        let input = RawSourceInput {
            source_ref: Some(DASHBOARD_SOURCE_REF.to_string()),
            format: SourceFormat::Json,
            content: serde_json::json!({
                "usageEventsDisplay": [{
                    "timestamp": "1785000000000",
                    "model": "claude-4.6-opus-high-thinking",
                    "isTokenBasedCall": true,
                    "tokenUsage": {
                        "inputTokens": 3,
                        "outputTokens": 20525,
                        "cacheWriteTokens": 112151,
                        "cacheReadTokens": 99,
                        "totalCents": 121.41
                    },
                    "chargedCents": 124.73
                }]
            })
            .to_string(),
        };

        let drafts = adapter.parse(&input).unwrap();
        assert_eq!(drafts.len(), 1);
        let draft = &drafts[0];
        assert_eq!(draft.tokens.input, TokenField::exact(112_154));
        assert_eq!(draft.tokens.output, TokenField::exact(20_525));
        assert_eq!(draft.tokens.cached_input, TokenField::exact(99));
        assert_eq!(draft.provider.as_deref(), Some("anthropic"));
        assert_eq!(draft.model.as_deref(), Some("claude-4.6-opus-high-thinking"));
        assert_eq!(
            draft.cost.as_ref().and_then(|cost| cost.amount.as_ref()),
            Some(&Money::new(12_473, "USD", 4).unwrap())
        );
        assert_eq!(
            draft.cost.as_ref().map(|cost| cost.status),
            Some(CostCalculationStatus::ReportedBySource)
        );
    }

    #[test]
    fn skips_lines_it_cannot_make_sense_of() {
        let adapter = CursorAdapter::new();
        let content = format!("not json\n{{\"type\":9}}\n{SAMPLE}");
        let input = RawSourceInput::new(content, SourceFormat::Jsonl);
        assert_eq!(adapter.parse(&input).unwrap().len(), 3);
    }

    #[test]
    fn parse_bubble_key_requires_two_uuids() {
        assert_eq!(
            parse_bubble_key("bubbleId:e09dc0ed-465b-4f71-9fb5-f3ba08e03288:ca97f535-e974-4685-8b70-b244b7502b0d"),
            Some((
                "e09dc0ed-465b-4f71-9fb5-f3ba08e03288".to_string(),
                "ca97f535-e974-4685-8b70-b244b7502b0d".to_string()
            ))
        );
        assert_eq!(parse_bubble_key("bubbleId:short:also-short"), None);
        assert_eq!(parse_bubble_key("agentKv:blob:abc"), None);
    }

    #[test]
    fn discovers_and_reads_a_sqlite_fixture() {
        let dir = std::env::temp_dir().join(format!("tokens-cursor-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("state.vscdb");

        {
            let connection = Connection::open(&db_path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value BLOB);
                     INSERT INTO cursorDiskKV VALUES (
                       'bubbleId:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb',
                       '{\"type\":1,\"createdAt\":\"2026-07-29T12:00:00.000Z\",\"requestId\":\"req-1\",\"modelInfo\":{\"modelName\":\"grok-4.5\"},\"tokenCount\":{\"inputTokens\":0,\"outputTokens\":0},\"text\":\"hi\"}'
                     );
                     INSERT INTO cursorDiskKV VALUES (
                       'bubbleId:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:cccccccc-cccc-cccc-cccc-cccccccccccc',
                       '{\"type\":2,\"createdAt\":\"2026-07-29T12:00:01.000Z\",\"requestId\":\"\",\"tokenCount\":{\"inputTokens\":0,\"outputTokens\":0},\"text\":\"hello\"}'
                     );
                     INSERT INTO cursorDiskKV VALUES ('agentKv:blob:deadbeef', 'ignored');",
                )
                .unwrap();
        }

        let adapter = CursorAdapter::with_db(db_path);
        let sources = adapter.discover().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].format, SourceFormat::Sqlite);

        let input = adapter.read(&sources[0]).unwrap();
        assert_eq!(input.format, SourceFormat::Jsonl);
        let drafts = adapter.parse(&input).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].model.as_deref(), Some("grok-4.5"));
        assert_eq!(drafts[0].source_event_id.as_deref(), Some("req-1"));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_current_period_usage_into_a_billing_cycle_quota() {
        let observed_at =
            DateTime::parse_from_rfc3339("2026-07-29T21:00:00Z").unwrap().to_utc();
        let value = serde_json::json!({
            "billingCycleStart": "2026-07-01T00:00:00Z",
            "billingCycleEnd": "2026-08-01T00:00:00Z",
            "planUsage": {
                "totalSpend": 23222,
                "limit": 40000,
                "autoPercentUsed": 3.0,
                "apiPercentUsed": 12.0,
                "totalPercentUsed": 4.4
            }
        });

        let quotas = parse_usage_response(&value, observed_at).unwrap();
        assert_eq!(quotas.len(), 2);
        assert_eq!(quotas[0].source_app, SourceApp::Cursor);
        assert_eq!(quotas[0].label.as_deref(), Some("Cursor Models"));
        assert_eq!(quotas[0].window_minutes, 31 * 24 * 60);
        assert_eq!(quotas[0].used_percent_tenths, 30);
        assert_eq!(quotas[0].remaining_percent_tenths(), 970);
        assert_eq!(quotas[1].label.as_deref(), Some("Other Models"));
        assert_eq!(quotas[1].used_percent_tenths, 120);
        assert_eq!(
            quotas[0].resets_at.unwrap().to_rfc3339(),
            "2026-08-01T00:00:00+00:00"
        );
        assert_eq!(quotas[0].observed_at, observed_at);
    }

    #[test]
    fn accepts_wrapped_responses_numeric_strings_and_api_fallback() {
        let observed_at = Utc::now();
        let value = serde_json::json!({
            "result": {
                "billingCycleStart": "1782864000000",
                "billingCycleEnd": "1785542400000",
                "planUsage": {
                    "apiPercentUsed": "46.444"
                }
            }
        });

        let quotas = parse_usage_response(&value, observed_at).unwrap();
        assert_eq!(quotas.len(), 1);
        assert_eq!(quotas[0].label.as_deref(), Some("Other Models"));
        assert_eq!(quotas[0].window_minutes, 31 * 24 * 60);
        assert_eq!(quotas[0].used_percent_tenths, 464);
    }

    #[test]
    fn rejects_usage_responses_without_a_reset_or_percentage() {
        assert!(parse_usage_response(&serde_json::json!({}), Utc::now()).is_none());
        assert!(
            parse_usage_response(
                &serde_json::json!({
                    "billingCycleEnd": "2026-08-01T00:00:00Z",
                    "planUsage": {}
                }),
                Utc::now()
            )
            .is_none()
        );
    }

    #[test]
    fn reports_discovery_failure_when_no_db_exists() {
        let adapter = CursorAdapter::with_db(PathBuf::from("/definitely/not/here.vscdb"));
        assert!(matches!(adapter.discover(), Err(AdapterError::Discovery { .. })));
    }

    fn dashboard_event(cents: f64, model: &str, timestamp: &str) -> serde_json::Value {
        serde_json::json!({
            "timestamp": timestamp,
            "model": model,
            "kind": "USAGE_EVENT_KIND_USAGE_BASED",
            "tokenUsage": { "inputTokens": 10, "outputTokens": 20 },
            "chargedCents": cents,
        })
    }

    fn parse_events(events: Vec<serde_json::Value>) -> Vec<UsageRecordDraft> {
        let adapter = CursorAdapter::new();
        adapter
            .parse(&RawSourceInput {
                source_ref: Some(DASHBOARD_SOURCE_REF.to_string()),
                format: SourceFormat::Json,
                content: serde_json::json!({ "usageEventsDisplay": events }).to_string(),
            })
            .unwrap()
    }

    #[test]
    fn a_corrected_cost_lands_on_the_event_it_corrects() {
        // Same event, re-published with the charge Cursor settled on. Identity
        // must not move, or the dashboard would show the charge twice.
        let before = parse_events(vec![dashboard_event(124.73, "opus", "1785000000000")]);
        let after = parse_events(vec![dashboard_event(98.10, "opus", "1785000000000")]);

        assert_eq!(before[0].source_event_id, after[0].source_event_id);
        assert_ne!(
            before[0].cost.as_ref().unwrap().amount,
            after[0].cost.as_ref().unwrap().amount
        );
    }

    #[test]
    fn a_server_identifier_is_authoritative_when_it_exists() {
        let with_id = |id: &str, cents: f64| {
            let mut event = dashboard_event(cents, "opus", "1785000000000");
            event["id"] = serde_json::Value::String(id.to_string());
            event
        };

        let drafts = parse_events(vec![with_id("evt-1", 100.0), with_id("evt-2", 100.0)]);
        // Two identical-looking events are still two events when the server
        // says so — the invariant fallback would have merged them.
        assert_eq!(drafts.len(), 2);
        assert_ne!(drafts[0].source_event_id, drafts[1].source_event_id);
        assert!(drafts[0]
            .source_event_id
            .as_deref()
            .is_some_and(|id| id.contains("evt-")));

        // And a correction to one of them keeps its identity.
        let corrected = parse_events(vec![with_id("evt-1", 55.0)]);
        assert_eq!(corrected[0].source_event_id, drafts[0].source_event_id);
    }

    #[test]
    fn events_differing_only_in_what_a_correction_can_move_share_an_identity() {
        // Without a server id, identity is built from what a correction may
        // not change. Two genuinely distinct events differ in one of those.
        let same = parse_events(vec![dashboard_event(1.0, "opus", "1785000000000")]);
        let other_model = parse_events(vec![dashboard_event(1.0, "sonnet", "1785000000000")]);
        let other_time = parse_events(vec![dashboard_event(1.0, "opus", "1785000009999")]);

        assert_ne!(same[0].source_event_id, other_model[0].source_event_id);
        assert_ne!(same[0].source_event_id, other_time[0].source_event_id);
    }

    #[test]
    fn a_fast_delta_reads_a_window_and_replaces_exactly_that_window() {
        // No dashboard session and no database: the request cannot reach a
        // source, which is the point — what is asserted here is the shape of
        // the checkpoint the delta path round-trips.
        let stored = DashboardCheckpoint {
            watermark_ms: Some(1_785_000_000_000),
            awaiting_upstream: true,
        };
        let request = DeltaRequest {
            mode: SyncMode::Incremental,
            checkpoints: vec![SourceCheckpoint {
                adapter_id: CursorAdapter::ID.to_string(),
                source_key: DASHBOARD_CHECKPOINT.to_string(),
                payload: serde_json::to_value(&stored).unwrap(),
            }],
            now: Utc::now(),
        };

        let resumed: DashboardCheckpoint = request.resume(DASHBOARD_CHECKPOINT).unwrap();
        assert_eq!(resumed.watermark_ms, stored.watermark_ms);
        assert!(resumed.awaiting_upstream);

        // The overlap looks *back* from the watermark, because Cursor
        // publishes out of order and corrects after the fact.
        let from = instant_ms(resumed.watermark_ms.unwrap()).unwrap() - DELTA_OVERLAP;
        assert!(from < instant_ms(stored.watermark_ms.unwrap()).unwrap());
    }

    #[test]
    fn an_unchanged_local_database_is_not_reopened() {
        let directory =
            std::env::temp_dir().join(format!("tokens-cursor-gate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        let db = directory.join("state.vscdb");
        fs::write(&db, b"not a database").unwrap();

        let adapter = CursorAdapter::with_db(db);
        let checkpoint = SourceCheckpoint {
            adapter_id: CursorAdapter::ID.to_string(),
            source_key: LOCAL_CHECKPOINT.to_string(),
            payload: serde_json::to_value(adapter.local_state()).unwrap(),
        };

        // The metadata gate answers before the file is opened — which is what
        // makes a burst of write notifications cost almost nothing. If it
        // opened this file it would fail, since it is not a database.
        let delta = adapter
            .read_delta(&DeltaRequest {
                mode: SyncMode::Incremental,
                checkpoints: vec![checkpoint],
                now: Utc::now(),
            })
            .unwrap();
        assert!(delta.is_empty());

        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn the_watched_root_covers_the_write_ahead_log() {
        let adapter = CursorAdapter::with_db(PathBuf::from("/apps/globalStorage/state.vscdb"));
        // The directory, not the file: SQLite writes through `-wal` siblings
        // and replaces files on checkpoint.
        assert_eq!(
            adapter.watch_roots(),
            vec![PathBuf::from("/apps/globalStorage")]
        );
        assert!(adapter.needs_network());
    }

    #[test]
    fn a_timestamp_is_read_in_either_form_cursor_has_used() {
        assert_eq!(
            parse_event_instant("1785000000000"),
            instant_ms(1_785_000_000_000)
        );
        assert!(parse_event_instant("2026-07-29T10:00:00Z").is_some());
        assert!(parse_event_instant("not a time").is_none());
    }
}
