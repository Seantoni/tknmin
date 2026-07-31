//! The persistent usage store.
//!
//! App-owned SQLite in WAL mode. Its job is to make a relaunch instant: the
//! window opens on the last committed revision and catch-up work happens
//! behind it, rather than the interface starting empty and waiting for a walk
//! over hundreds of megabytes of logs.
//!
//! Records are stored as their normalized JSON alongside the few columns
//! queries actually filter on. Aggregation stays in [`summarize`], shared with
//! the in-memory backend, so the two can be checked against each other rather
//! than drifting into two different definitions of a total.
//!
//! What is deliberately not stored: prompts, responses, filesystem paths, and
//! anything an adapter uses to authenticate. Checkpoints are adapter-defined
//! JSON, and adapters are required to keep credentials out of them.

use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};

use crate::domain::{
    group_threads, same_window_instance, QuotaSample, RecentQuery, RepositoryRevision, SourceApp,
    SourceCheckpoint, SourceSyncHealth, SummaryQuery, SyncState, ThreadUsageQuery,
    ThreadUsageReport, UsageQuota, UsageRecord, UsageSummary, WindowPace,
};

use super::summarize;
use super::{
    apply_health, assemble_pace_only, assemble_risk_snapshot, assemble_snapshot,
    materially_differs, merge_quota, quota_key, CommitCounts, CommitOutcome, DashboardSnapshot,
    RepositoryError, RiskSnapshot, SourceTransaction, UsageReader, UsageWriter,
};

/// Bumped whenever the stored shape changes. Migrations run in order and are
/// recorded, so an older install upgrades in place instead of starting over.
const SCHEMA_VERSION: u32 = 3;

/// The base shape, created for a fresh store. `quota_samples` is *not* here:
/// it arrives as migration 2, so a store written before it existed upgrades
/// to exactly the same shape a new store ends in.
const BASE_SCHEMA: &str = r#"
    CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS records (
        dedupe_key            TEXT PRIMARY KEY,
        content_hash          TEXT NOT NULL,
        source_app            TEXT NOT NULL,
        adapter_id            TEXT NOT NULL,
        event_timestamp_utc   INTEGER,
        normalization_version INTEGER NOT NULL,
        payload               TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS records_by_source_time
        ON records (source_app, event_timestamp_utc);
    CREATE INDEX IF NOT EXISTS records_by_adapter_time
        ON records (adapter_id, event_timestamp_utc);

    CREATE TABLE IF NOT EXISTS checkpoints (
        adapter_id TEXT NOT NULL,
        source_key TEXT NOT NULL,
        payload    TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (adapter_id, source_key)
    );

    CREATE TABLE IF NOT EXISTS quotas (
        source_app          TEXT NOT NULL,
        label               TEXT NOT NULL,
        window_minutes      INTEGER NOT NULL,
        used_percent_tenths INTEGER NOT NULL,
        resets_at           INTEGER,
        observed_at         INTEGER NOT NULL,
        PRIMARY KEY (source_app, label, window_minutes)
    );

    CREATE TABLE IF NOT EXISTS source_health (
        source_app         TEXT PRIMARY KEY,
        state              TEXT NOT NULL,
        app_synced_at      INTEGER,
        source_observed_at INTEGER,
        last_attempt_at    INTEGER,
        last_error         TEXT,
        awaiting_upstream  INTEGER NOT NULL DEFAULT 0
    );
"#;

/// Ordered migrations, applied in sequence after the base schema. Each runs
/// inside its own transaction together with the version stamp that records
/// it, so an upgrade either lands a whole step or none of it.
///
/// `quota_samples` holds one historical observation per quota change. The
/// quota lane polls about once a minute but a row is written only when the
/// value moves, so the table holds deltas rather than polls. `resets_at` is
/// carried per row because it identifies the window instance: a delta is only
/// ever taken between samples sharing one. All instants are milliseconds since
/// the Unix epoch, like every other timestamp in this schema.
///
/// Migration 3 adds `records.source_ref`, so a replace scope can name one
/// dataset within an adapter rather than all of them. It is denormalised out
/// of the payload because the scope query filters on it, and deserialising
/// every record in the store to answer that would defeat the index.
///
/// Existing rows are backfilled from the payload they already carry, so a
/// store written before this column existed ends in exactly the shape a new
/// one does. A row whose payload names no `source_ref` keeps SQL `NULL`,
/// which is what a scope that names no dataset matches.
const MIGRATIONS: &[(u32, &str)] = &[
    (
        2,
        r#"
    CREATE TABLE IF NOT EXISTS quota_samples (
        source_app          TEXT NOT NULL,
        label               TEXT NOT NULL,
        window_minutes      INTEGER NOT NULL,
        used_percent_tenths INTEGER NOT NULL,
        resets_at           INTEGER,
        observed_at         INTEGER NOT NULL,
        PRIMARY KEY (source_app, label, window_minutes, observed_at)
    );
    CREATE INDEX IF NOT EXISTS quota_samples_recent
        ON quota_samples (source_app, label, window_minutes, observed_at DESC);
    "#,
    ),
    (
        3,
        r#"
    ALTER TABLE records ADD COLUMN source_ref TEXT;
    UPDATE records
       SET source_ref = json_extract(payload, '$.provenance.sourceRef')
     WHERE source_ref IS NULL;
    CREATE INDEX IF NOT EXISTS records_by_dataset_time
        ON records (source_app, adapter_id, source_ref, event_timestamp_utc);
    "#,
    ),
];

const META_REVISION: &str = "revision";

pub struct SqliteUsageRepository {
    /// One connection, serialized. Every write is a transaction and every read
    /// is short, so contention is not the bottleneck — correctness is.
    connection: Mutex<Connection>,
}

impl SqliteUsageRepository {
    /// Open (or create) the store at `path`, running any pending migrations.
    pub fn open(path: &Path) -> Result<Self, RepositoryError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| backend(&error))?;
        }
        let connection = Connection::open(path).map_err(|error| backend(&error))?;
        Self::prepare(connection)
    }

    /// An unshared in-memory database, for tests that still want the real SQL.
    pub fn in_memory() -> Result<Self, RepositoryError> {
        let connection = Connection::open_in_memory().map_err(|error| backend(&error))?;
        Self::prepare(connection)
    }

    fn prepare(mut connection: Connection) -> Result<Self, RepositoryError> {
        // WAL lets the interface read while a source commits. `NORMAL` is the
        // right durability for derived data: the logs on disk remain the
        // authority, so the worst a lost transaction costs is a re-read.
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .map_err(|error| backend(&error))?;
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| backend(&error))?;
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|error| backend(&error))?;

        migrate(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn with_connection<T>(
        &self,
        body: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
    ) -> Result<T, RepositoryError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| RepositoryError::Unavailable)?;
        body(&connection).map_err(|error| backend(&error))
    }

    /// Load every record a query could match, filtered in SQL where the
    /// filter is indexable and in Rust where it is not.
    fn load_matching(
        connection: &Connection,
        query: &SummaryQuery,
    ) -> Result<Vec<UsageRecord>, rusqlite::Error> {
        let mut statement = connection.prepare("SELECT payload FROM records")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;

        let mut records = Vec::new();
        for row in rows {
            let payload = row?;
            // A row this build cannot deserialize is from a newer schema or a
            // corrupt write. Skipping it keeps the rest of the dashboard alive.
            if let Ok(record) = serde_json::from_str::<UsageRecord>(&payload) {
                if summarize::matches(&record, query) {
                    records.push(record);
                }
            }
        }
        Ok(records)
    }

    fn load_all(connection: &Connection) -> Result<Vec<UsageRecord>, rusqlite::Error> {
        Self::load_matching(connection, &SummaryQuery::default())
    }
}

fn migrate(connection: &mut Connection) -> Result<(), RepositoryError> {
    connection
        .execute_batch(BASE_SCHEMA)
        .map_err(|error| backend(&error))?;

    connection
        .execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '1'), ('revision', '0')",
            [],
        )
        .map_err(|error| backend(&error))?;

    // A store written by a newer build may hold columns this one cannot
    // interpret. Refusing is safer than silently dropping fields on write.
    let stored: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| backend(&error))?;
    let stored: u32 = stored.parse().unwrap_or(0);
    if stored > SCHEMA_VERSION {
        return Err(RepositoryError::Backend(format!(
            "the usage store was written by a newer version of Tokens (schema {stored})"
        )));
    }

    // Apply each pending migration in order, stamping its version inside the
    // same transaction so a step either lands whole or not at all.
    for (version, sql) in MIGRATIONS {
        if stored >= *version {
            continue;
        }
        let transaction = connection.transaction().map_err(|error| backend(&error))?;
        transaction
            .execute_batch(sql)
            .map_err(|error| backend(&error))?;
        transaction
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'schema_version'",
                params![version.to_string()],
            )
            .map_err(|error| backend(&error))?;
        transaction.commit().map_err(|error| backend(&error))?;
    }

    Ok(())
}

impl UsageReader for SqliteUsageRepository {
    fn summary(&self, query: &SummaryQuery) -> Result<UsageSummary, RepositoryError> {
        self.with_connection(|connection| {
            let all = SqliteUsageRepository::load_all(connection)?;
            let undated = summarize::undated_excluded(all.iter(), query);
            let matching = all
                .iter()
                .filter(|record| summarize::matches(record, query));
            Ok(summarize::summarize(matching, Utc::now(), undated))
        })
    }

    fn recent(&self, query: &RecentQuery) -> Result<Vec<UsageRecord>, RepositoryError> {
        self.with_connection(|connection| {
            let matching = SqliteUsageRepository::load_matching(connection, &query.filter)?;
            Ok(summarize::take_recent(matching.iter().collect(), query))
        })
    }

    fn thread_usage(&self, query: &ThreadUsageQuery) -> Result<ThreadUsageReport, RepositoryError> {
        self.with_connection(|connection| {
            let undated_all = SqliteUsageRepository::load_all(connection)?;
            let undated = summarize::undated_excluded(undated_all.iter(), &query.filter);
            let matching = SqliteUsageRepository::load_matching(connection, &query.filter)?;
            Ok(group_threads(
                matching.iter(),
                Utc::now(),
                undated,
                query.effective_limit(),
            ))
        })
    }

    fn records_since(&self, since: DateTime<Utc>) -> Result<Vec<UsageRecord>, RepositoryError> {
        self.with_connection(|connection| {
            // The one read that bounds itself in SQL: `event_timestamp_utc` is
            // indexed, so a month of history costs a range scan rather than
            // deserializing every payload in the store.
            let mut statement = connection
                .prepare("SELECT payload FROM records WHERE event_timestamp_utc >= ?1")?;
            let rows = statement.query_map(params![since.timestamp_millis()], |row| {
                row.get::<_, String>(0)
            })?;
            let mut records = Vec::new();
            for row in rows {
                // A row this build cannot deserialize is from a newer schema or
                // a corrupt write; skipping it keeps the rest alive.
                if let Ok(record) = serde_json::from_str::<UsageRecord>(&row?) {
                    records.push(record);
                }
            }
            Ok(records)
        })
    }

    fn count(&self) -> Result<usize, RepositoryError> {
        self.with_connection(|connection| {
            let count: i64 =
                connection.query_row("SELECT COUNT(*) FROM records", [], |row| row.get(0))?;
            Ok(count as usize)
        })
    }

    fn quotas(&self) -> Result<Vec<UsageQuota>, RepositoryError> {
        self.with_connection(read_quotas)
    }

    fn quota_samples(&self, since: DateTime<Utc>) -> Result<Vec<QuotaSample>, RepositoryError> {
        self.with_connection(|connection| {
            let mut statement = connection.prepare(
                "SELECT source_app, label, window_minutes, used_percent_tenths, resets_at, observed_at
                 FROM quota_samples WHERE observed_at >= ?1
                 ORDER BY source_app, label, window_minutes, observed_at",
            )?;
            let rows = statement.query_map(params![since.timestamp_millis()], |row| {
                let source: String = row.get(0)?;
                let label: String = row.get(1)?;
                Ok(SourceApp::from_str(&source).ok().map(|source_app| QuotaSample {
                    source_app,
                    label: (!label.is_empty()).then_some(label),
                    window_minutes: row.get::<_, i64>(2).unwrap_or_default() as u32,
                    used_percent_tenths: row.get::<_, i64>(3).unwrap_or_default() as u16,
                    resets_at: row.get::<_, Option<i64>>(4).unwrap_or_default().and_then(instant),
                    observed_at: instant(row.get::<_, i64>(5).unwrap_or_default())
                        .unwrap_or_else(Utc::now),
                }))
            })?;
            Ok(rows.filter_map(|row| row.ok().flatten()).collect())
        })
    }

    fn health(&self) -> Result<Vec<SourceSyncHealth>, RepositoryError> {
        self.with_connection(read_health)
    }

    fn revision(&self) -> Result<RepositoryRevision, RepositoryError> {
        self.with_connection(read_revision)
    }

    fn checkpoints(&self, adapter_id: &str) -> Result<Vec<SourceCheckpoint>, RepositoryError> {
        self.with_connection(|connection| {
            let mut statement = connection
                .prepare("SELECT source_key, payload FROM checkpoints WHERE adapter_id = ?1")?;
            let rows = statement.query_map(params![adapter_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut checkpoints = Vec::new();
            for row in rows {
                let (source_key, payload) = row?;
                if let Ok(payload) = serde_json::from_str(&payload) {
                    checkpoints.push(SourceCheckpoint {
                        adapter_id: adapter_id.to_string(),
                        source_key,
                        payload,
                    });
                }
            }
            Ok(checkpoints)
        })
    }

    fn snapshot(
        &self,
        overview: &SummaryQuery,
        recent: &RecentQuery,
    ) -> Result<DashboardSnapshot, RepositoryError> {
        assemble_snapshot(self, overview, recent)
    }

    fn risk_snapshot(&self) -> Result<RiskSnapshot, RepositoryError> {
        assemble_risk_snapshot(self)
    }

    fn pace(&self) -> Result<Vec<WindowPace>, RepositoryError> {
        assemble_pace_only(self)
    }
}

impl UsageWriter for SqliteUsageRepository {
    fn commit(&self, batch: SourceTransaction) -> Result<CommitOutcome, RepositoryError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| RepositoryError::Unavailable)?;
        let transaction = connection.transaction().map_err(|error| backend(&error))?;

        let outcome = apply(&transaction, batch).map_err(|error| backend(&error))?;

        // Rolling back on failure is the point of the whole shape: a
        // checkpoint that survived a failed record write would claim data the
        // store does not have, and the next run would skip past it forever.
        transaction.commit().map_err(|error| backend(&error))?;
        Ok(outcome)
    }

    fn prune_quota_samples(&self, before: DateTime<Utc>) -> Result<usize, RepositoryError> {
        self.with_connection(|connection| {
            connection.execute(
                "DELETE FROM quota_samples WHERE observed_at < ?1",
                params![before.timestamp_millis()],
            )
        })
    }
}

fn apply(
    transaction: &Transaction<'_>,
    batch: SourceTransaction,
) -> Result<CommitOutcome, rusqlite::Error> {
    let mut counts = CommitCounts::default();

    let keep: Vec<&str> = batch
        .records
        .iter()
        .map(|record| record.dedupe_key.as_str())
        .collect();
    for scope in &batch.replace_scopes {
        // A scope naming no dataset covers all of them, so the `?5 IS NULL`
        // arm is the whole filter for a single-dataset adapter and costs one
        // constant comparison.
        let mut statement = transaction.prepare(
            "SELECT dedupe_key FROM records
             WHERE source_app = ?1 AND adapter_id = ?2
               AND (?5 IS NULL OR source_ref IS ?5)
               AND event_timestamp_utc IS NOT NULL
               AND event_timestamp_utc >= ?3 AND event_timestamp_utc < ?4",
        )?;
        let doomed: Vec<String> = statement
            .query_map(
                params![
                    scope.source_app.as_str(),
                    scope.adapter_id,
                    scope.from.timestamp_millis(),
                    scope.until.timestamp_millis(),
                    scope.source_ref,
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<_, _>>()?;
        for key in doomed {
            if keep.contains(&key.as_str()) {
                continue;
            }
            transaction.execute("DELETE FROM records WHERE dedupe_key = ?1", params![key])?;
            counts.deleted += 1;
        }
    }

    for record in &batch.records {
        let stored: Option<String> = transaction
            .query_row(
                "SELECT content_hash FROM records WHERE dedupe_key = ?1",
                params![record.dedupe_key],
                |row| row.get(0),
            )
            .optional()?;

        match stored {
            Some(hash) if hash == record.content_hash => {
                counts.unchanged += 1;
                continue;
            }
            Some(_) => counts.updated += 1,
            None => counts.inserted += 1,
        }

        let payload = serde_json::to_string(record)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        transaction.execute(
            "INSERT INTO records
                (dedupe_key, content_hash, source_app, adapter_id, source_ref,
                 event_timestamp_utc, normalization_version, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(dedupe_key) DO UPDATE SET
                content_hash = excluded.content_hash,
                source_app = excluded.source_app,
                adapter_id = excluded.adapter_id,
                source_ref = excluded.source_ref,
                event_timestamp_utc = excluded.event_timestamp_utc,
                normalization_version = excluded.normalization_version,
                payload = excluded.payload",
            params![
                record.dedupe_key,
                record.content_hash,
                record.source_app.as_str(),
                record.provenance.adapter_id,
                record.provenance.source_ref,
                record.event_timestamp_utc.map(|at| at.timestamp_millis()),
                record.normalization_version,
                payload,
            ],
        )?;
    }

    let mut quotas_changed = false;
    if batch.replace_quotas {
        let reported: Vec<(SourceApp, Option<String>, u32)> =
            batch.quotas.iter().map(quota_key).collect();
        let stored = read_quotas(transaction)?;
        for quota in stored
            .iter()
            .filter(|stored| stored.source_app == batch.source_app)
        {
            if reported.contains(&quota_key(quota)) {
                continue;
            }
            transaction.execute(
                "DELETE FROM quotas
                 WHERE source_app = ?1 AND label = ?2 AND window_minutes = ?3",
                params![
                    quota.source_app.as_str(),
                    quota.label.clone().unwrap_or_default(),
                    quota.window_minutes,
                ],
            )?;
            quotas_changed = true;
        }
    }
    for incoming in batch.quotas {
        // Read-modify-write inside the transaction, so the source's own
        // observation time — not arrival order — decides the winner.
        let mut stored = read_quotas(transaction)?
            .into_iter()
            .filter(|quota| quota_key(quota) == quota_key(&incoming))
            .collect::<Vec<_>>();
        if !merge_quota(&mut stored, incoming.clone()) {
            continue;
        }
        quotas_changed = true;
        transaction.execute(
            "INSERT INTO quotas
                (source_app, label, window_minutes, used_percent_tenths, resets_at, observed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(source_app, label, window_minutes) DO UPDATE SET
                used_percent_tenths = excluded.used_percent_tenths,
                resets_at = excluded.resets_at,
                observed_at = excluded.observed_at",
            params![
                incoming.source_app.as_str(),
                incoming.label.clone().unwrap_or_default(),
                incoming.window_minutes,
                incoming.used_percent_tenths,
                incoming.resets_at.map(|at| at.timestamp_millis()),
                incoming.observed_at.timestamp_millis(),
            ],
        )?;

        // Sample for pace history — only when the value or the window
        // instance moved, so the table holds deltas rather than polls. The
        // live quota row carries the advancing observation time; the sample
        // table only needs the points where something actually changed.
        let label = incoming.label.clone().unwrap_or_default();
        let last: Option<(i64, Option<i64>)> = transaction
            .query_row(
                "SELECT used_percent_tenths, resets_at FROM quota_samples
                 WHERE source_app = ?1 AND label = ?2 AND window_minutes = ?3
                 ORDER BY observed_at DESC LIMIT 1",
                params![incoming.source_app.as_str(), label, incoming.window_minutes],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let moved = last.is_none_or(|(used, resets)| {
            used != i64::from(incoming.used_percent_tenths)
                // A restated reset jitters by a second or so. Compared
                // exactly it looks like a new window on every poll, and the
                // table fills with rows that record nothing.
                || !same_window_instance(
                    resets.and_then(|at| DateTime::from_timestamp_millis(at)),
                    incoming.resets_at,
                )
        });
        if moved {
            transaction.execute(
                "INSERT INTO quota_samples
                    (source_app, label, window_minutes, used_percent_tenths, resets_at, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    incoming.source_app.as_str(),
                    label,
                    incoming.window_minutes,
                    incoming.used_percent_tenths,
                    incoming.resets_at.map(|at| at.timestamp_millis()),
                    incoming.observed_at.timestamp_millis(),
                ],
            )?;
        }
    }

    for checkpoint in &batch.checkpoints {
        let payload = serde_json::to_string(&checkpoint.payload)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        transaction.execute(
            "INSERT INTO checkpoints (adapter_id, source_key, payload, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(adapter_id, source_key) DO UPDATE SET
                payload = excluded.payload,
                updated_at = excluded.updated_at",
            params![
                checkpoint.adapter_id,
                checkpoint.source_key,
                payload,
                batch.committed_at.timestamp_millis(),
            ],
        )?;
    }

    let stored_health = read_health(transaction)?;
    let mut health = stored_health
        .into_iter()
        .find(|health| health.source_app == batch.source_app)
        .unwrap_or_else(|| SourceSyncHealth::unknown(batch.source_app));
    let before = health.clone();
    apply_health(&mut health, &batch.health, batch.committed_at);
    let health_changed = materially_differs(&health, &before);
    // The row is written whenever anything on it moved, including the
    // attempt time, so freshness stays accurate even when the revision does
    // not advance.
    if health != before {
        transaction.execute(
            "INSERT INTO source_health
                (source_app, state, app_synced_at, source_observed_at,
                 last_attempt_at, last_error, awaiting_upstream)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(source_app) DO UPDATE SET
                state = excluded.state,
                app_synced_at = excluded.app_synced_at,
                source_observed_at = excluded.source_observed_at,
                last_attempt_at = excluded.last_attempt_at,
                last_error = excluded.last_error,
                awaiting_upstream = excluded.awaiting_upstream",
            params![
                health.source_app.as_str(),
                state_name(health.state),
                health.app_synced_at.map(|at| at.timestamp_millis()),
                health.source_observed_at.map(|at| at.timestamp_millis()),
                health.last_attempt_at.map(|at| at.timestamp_millis()),
                health.last_error,
                health.awaiting_upstream as i64,
            ],
        )?;
    }

    let data_changed = counts.changed_records() || quotas_changed;
    let advanced = data_changed || health_changed;
    let mut revision = read_revision(transaction)?;
    if advanced {
        revision += 1;
        transaction.execute(
            "UPDATE meta SET value = ?1 WHERE key = ?2",
            params![revision.to_string(), META_REVISION],
        )?;
    }

    Ok(CommitOutcome {
        revision,
        counts,
        advanced,
        data_changed,
    })
}

fn read_revision(connection: &Connection) -> Result<RepositoryRevision, rusqlite::Error> {
    let value: String = connection.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![META_REVISION],
        |row| row.get(0),
    )?;
    Ok(value.parse().unwrap_or(0))
}

fn read_quotas(connection: &Connection) -> Result<Vec<UsageQuota>, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT source_app, label, window_minutes, used_percent_tenths, resets_at, observed_at
         FROM quotas ORDER BY source_app, window_minutes, label",
    )?;
    let rows = statement.query_map([], |row| {
        let source: String = row.get(0)?;
        let label: String = row.get(1)?;
        Ok(SourceApp::from_str(&source)
            .ok()
            .map(|source_app| UsageQuota {
                source_app,
                label: (!label.is_empty()).then_some(label),
                window_minutes: row.get::<_, i64>(2).unwrap_or_default() as u32,
                used_percent_tenths: row.get::<_, i64>(3).unwrap_or_default() as u16,
                resets_at: row
                    .get::<_, Option<i64>>(4)
                    .unwrap_or_default()
                    .and_then(instant),
                observed_at: instant(row.get::<_, i64>(5).unwrap_or_default())
                    .unwrap_or_else(Utc::now),
            }))
    })?;
    Ok(rows.filter_map(|row| row.ok().flatten()).collect())
}

fn read_health(connection: &Connection) -> Result<Vec<SourceSyncHealth>, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT source_app, state, app_synced_at, source_observed_at,
                last_attempt_at, last_error, awaiting_upstream
         FROM source_health ORDER BY source_app",
    )?;
    let rows = statement.query_map([], |row| {
        let source: String = row.get(0)?;
        let state: String = row.get(1)?;
        Ok(SourceApp::from_str(&source)
            .ok()
            .map(|source_app| SourceSyncHealth {
                source_app,
                state: state_from_name(&state),
                app_synced_at: row
                    .get::<_, Option<i64>>(2)
                    .unwrap_or_default()
                    .and_then(instant),
                source_observed_at: row
                    .get::<_, Option<i64>>(3)
                    .unwrap_or_default()
                    .and_then(instant),
                last_attempt_at: row
                    .get::<_, Option<i64>>(4)
                    .unwrap_or_default()
                    .and_then(instant),
                last_error: row.get::<_, Option<String>>(5).unwrap_or_default(),
                awaiting_upstream: row.get::<_, i64>(6).unwrap_or_default() != 0,
            }))
    })?;
    Ok(rows.filter_map(|row| row.ok().flatten()).collect())
}

fn instant(millis: i64) -> Option<DateTime<Utc>> {
    Utc.timestamp_millis_opt(millis).single()
}

fn state_name(state: SyncState) -> &'static str {
    match state {
        SyncState::Unknown => "unknown",
        SyncState::Current => "current",
        SyncState::Syncing => "syncing",
        SyncState::Stale => "stale",
        SyncState::Offline => "offline",
        SyncState::Error => "error",
    }
}

fn state_from_name(name: &str) -> SyncState {
    match name {
        "current" => SyncState::Current,
        "syncing" => SyncState::Syncing,
        "stale" => SyncState::Stale,
        "offline" => SyncState::Offline,
        "error" => SyncState::Error,
        _ => SyncState::Unknown,
    }
}

fn backend(error: &dyn std::fmt::Display) -> RepositoryError {
    RepositoryError::Backend(error.to_string())
}

/// A failed transaction must leave the store exactly as it was, including its
/// revision — otherwise the interface would refetch for a change that is not
/// there, and a checkpoint could outlive the records it claims.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        ReplaceScope, SourceProvenance, TokenCounts, TokenField, UsageRecordDraft,
    };
    use crate::normalize;
    use crate::repository::HealthOutcome;

    fn provenance(adapter: &str) -> SourceProvenance {
        SourceProvenance {
            adapter_id: adapter.to_string(),
            adapter_version: "1.0.0".to_string(),
            source_ref: None,
        }
    }

    fn record(event_id: &str, day: u32, output: u64) -> UsageRecord {
        let draft = UsageRecordDraft::new(SourceApp::Codex, provenance("codex"))
            .with_source_event_id(event_id)
            .with_raw_timestamp(format!("2026-07-{day:02}T10:00:00Z"))
            .with_tokens(TokenCounts {
                input: TokenField::exact(100),
                output: TokenField::exact(output),
                ..TokenCounts::default()
            });
        normalize::normalize(draft).unwrap()
    }

    fn usage(records: Vec<UsageRecord>) -> SourceTransaction {
        SourceTransaction {
            records,
            health: HealthOutcome::Succeeded {
                source_observed_at: None,
                awaiting_upstream: false,
            },
            ..SourceTransaction::new(SourceApp::Codex, "codex", Utc::now())
        }
    }

    fn store() -> SqliteUsageRepository {
        SqliteUsageRepository::in_memory().unwrap()
    }

    #[test]
    fn a_committed_batch_survives_reopening_the_file() {
        let directory = std::env::temp_dir().join(format!("tokens-test-{}", std::process::id()));
        let path = directory.join("usage.sqlite3");
        let _ = std::fs::remove_dir_all(&directory);

        {
            let repository = SqliteUsageRepository::open(&path).unwrap();
            repository.commit(usage(vec![record("e1", 1, 10)])).unwrap();
        }

        let reopened = SqliteUsageRepository::open(&path).unwrap();
        assert_eq!(reopened.count().unwrap(), 1);
        assert_eq!(reopened.revision().unwrap(), 1);
        assert_eq!(reopened.health().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn replaying_identical_content_is_a_no_op() {
        let repository = store();
        repository.commit(usage(vec![record("e1", 1, 10)])).unwrap();
        let second = repository.commit(usage(vec![record("e1", 1, 10)])).unwrap();

        assert_eq!(second.counts.unchanged, 1);
        assert_eq!(second.counts.inserted, 0);
        assert_eq!(repository.count().unwrap(), 1);
    }

    #[test]
    fn an_interim_snapshot_is_replaced_by_its_final_one() {
        let repository = store();
        repository.commit(usage(vec![record("e1", 1, 5)])).unwrap();
        let settled = repository
            .commit(usage(vec![record("e1", 1, 120)]))
            .unwrap();

        assert_eq!(settled.counts.updated, 1);
        assert_eq!(repository.count().unwrap(), 1);
        let summary = repository.summary(&SummaryQuery::default()).unwrap();
        assert_eq!(summary.totals.output.tokens, 120);
    }

    #[test]
    fn a_corrected_event_updates_one_row_rather_than_creating_two() {
        let repository = store();
        repository.commit(usage(vec![record("e1", 1, 10)])).unwrap();
        repository
            .commit(SourceTransaction {
                replace_scopes: vec![ReplaceScope {
                    source_app: SourceApp::Codex,
                    adapter_id: "codex".to_string(),
                    source_ref: None,
                    from: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
                    until: Utc.with_ymd_and_hms(2026, 7, 2, 0, 0, 0).unwrap(),
                }],
                ..usage(vec![record("e1", 1, 99)])
            })
            .unwrap();

        assert_eq!(repository.count().unwrap(), 1);
        let summary = repository.summary(&SummaryQuery::default()).unwrap();
        assert_eq!(summary.totals.output.tokens, 99);
    }

    #[test]
    fn a_replace_scope_only_touches_its_own_adapters_rows() {
        let repository = store();
        let mine = record("mine", 5, 10);
        let mut theirs = record("theirs", 5, 10);
        theirs.provenance.adapter_id = "cursor_local".to_string();
        repository.commit(usage(vec![mine, theirs])).unwrap();

        let outcome = repository
            .commit(SourceTransaction {
                replace_scopes: vec![ReplaceScope {
                    source_app: SourceApp::Codex,
                    adapter_id: "codex".to_string(),
                    source_ref: None,
                    from: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
                    until: Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
                }],
                ..usage(vec![])
            })
            .unwrap();

        assert_eq!(outcome.counts.deleted, 1);
        assert_eq!(repository.count().unwrap(), 1);
    }

    #[test]
    fn a_replace_scope_only_touches_its_own_dataset() {
        // One adapter reading its source two ways at once — Cursor's billed
        // events and its local activity. Without the dataset in the scope each
        // pass would sweep away the other's rows, so the two would take turns
        // deleting each other for as long as both kept running.
        let repository = store();
        let mut billed = record("billed", 5, 10);
        billed.provenance.source_ref = Some("dashboard-usage-events".to_string());
        let mut live = record("live", 5, 10);
        live.provenance.source_ref = Some("local-activity".to_string());
        repository.commit(usage(vec![billed, live])).unwrap();

        let outcome = repository
            .commit(SourceTransaction {
                replace_scopes: vec![ReplaceScope {
                    source_app: SourceApp::Codex,
                    adapter_id: "codex".to_string(),
                    source_ref: Some("dashboard-usage-events".to_string()),
                    from: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
                    until: Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
                }],
                ..usage(vec![])
            })
            .unwrap();

        assert_eq!(outcome.counts.deleted, 1, "only the billed row");
        assert_eq!(repository.count().unwrap(), 1);

        // And a scope naming no dataset still covers every one of them.
        let outcome = repository
            .commit(SourceTransaction {
                replace_scopes: vec![ReplaceScope {
                    source_app: SourceApp::Codex,
                    adapter_id: "codex".to_string(),
                    source_ref: None,
                    from: Utc.with_ymd_and_hms(2026, 7, 1, 0, 0, 0).unwrap(),
                    until: Utc.with_ymd_and_hms(2026, 8, 1, 0, 0, 0).unwrap(),
                }],
                ..usage(vec![])
            })
            .unwrap();
        assert_eq!(outcome.counts.deleted, 1);
        assert_eq!(repository.count().unwrap(), 0);
    }

    #[test]
    fn revisions_increase_monotonically_and_only_on_change() {
        let repository = store();
        assert_eq!(repository.revision().unwrap(), 0);

        repository.commit(usage(vec![record("e1", 1, 10)])).unwrap();
        let after_insert = repository.revision().unwrap();
        assert_eq!(after_insert, 1);

        let at = Utc.timestamp_opt(1_800_000_000, 0).unwrap();
        let repeat = || SourceTransaction {
            committed_at: at,
            ..usage(vec![record("e1", 1, 10)])
        };
        repository.commit(repeat()).unwrap();
        let settled = repository.revision().unwrap();
        let again = repository.commit(repeat()).unwrap();

        assert!(!again.advanced);
        assert_eq!(again.revision, settled);
    }

    #[test]
    fn an_older_quota_observation_cannot_replace_a_newer_one() {
        let repository = store();
        let quota = |used: u16, observed: i64| UsageQuota {
            source_app: SourceApp::Codex,
            label: None,
            window_minutes: 10_080,
            used_percent_tenths: used,
            resets_at: None,
            observed_at: Utc.timestamp_opt(observed, 0).unwrap(),
        };

        for snapshot in [quota(500, 2_000), quota(100, 1_000)] {
            repository
                .commit(SourceTransaction {
                    quotas: vec![snapshot],
                    ..SourceTransaction::new(SourceApp::Codex, "codex", Utc::now())
                })
                .unwrap();
        }

        let stored = repository.quotas().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].used_percent_tenths, 500);
    }

    #[test]
    fn a_failed_attempt_keeps_the_last_good_quota() {
        let repository = store();
        repository
            .commit(SourceTransaction {
                quotas: vec![UsageQuota {
                    source_app: SourceApp::Codex,
                    label: None,
                    window_minutes: 10_080,
                    used_percent_tenths: 400,
                    resets_at: None,
                    observed_at: Utc::now(),
                }],
                replace_quotas: true,
                ..SourceTransaction::new(SourceApp::Codex, "codex", Utc::now())
            })
            .unwrap();

        repository
            .commit(SourceTransaction::health_only(
                SourceApp::Codex,
                "codex",
                HealthOutcome::Failed {
                    offline: true,
                    error: "network unreachable".to_string(),
                },
                Utc::now(),
            ))
            .unwrap();

        let stored = repository.quotas().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].used_percent_tenths, 400);
        assert_eq!(repository.health().unwrap()[0].state, SyncState::Offline);
    }

    #[test]
    fn checkpoints_are_scoped_to_their_adapter() {
        let repository = store();
        repository
            .commit(SourceTransaction {
                checkpoints: vec![SourceCheckpoint {
                    adapter_id: "codex".to_string(),
                    source_key: "rollout-1".to_string(),
                    payload: serde_json::json!({ "offset": 128, "ordinal": 4 }),
                }],
                ..SourceTransaction::new(SourceApp::Codex, "codex", Utc::now())
            })
            .unwrap();

        let stored = repository.checkpoints("codex").unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].payload["ordinal"], 4);
        assert!(repository.checkpoints("claude_code").unwrap().is_empty());
    }

    #[test]
    fn quota_samples_are_written_only_when_the_value_moves() {
        let repository = store();
        let quota = |used: u16, observed: i64| UsageQuota {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: 300,
            used_percent_tenths: used,
            resets_at: Some(Utc.timestamp_opt(10_000, 0).unwrap()),
            observed_at: Utc.timestamp_opt(observed, 0).unwrap(),
        };
        let commit = |snapshot: UsageQuota| {
            repository
                .commit(SourceTransaction {
                    quotas: vec![snapshot],
                    ..SourceTransaction::new(SourceApp::ClaudeCode, "claude", Utc::now())
                })
                .unwrap();
        };

        commit(quota(100, 1_000));
        commit(quota(100, 1_100)); // same value, fresher read: no new sample
        commit(quota(150, 1_200)); // value moved: a new sample
        commit(quota(150, 1_300)); // same again: no new sample

        let samples = repository
            .quota_samples(Utc.timestamp_opt(0, 0).unwrap())
            .unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].used_percent_tenths, 100);
        assert_eq!(samples[1].used_percent_tenths, 150);
    }

    #[test]
    fn prune_quota_samples_drops_only_old_rows() {
        let repository = store();
        let quota = |used: u16, observed: i64| UsageQuota {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: 300,
            used_percent_tenths: used,
            resets_at: Some(Utc.timestamp_opt(1_000_000, 0).unwrap()),
            observed_at: Utc.timestamp_opt(observed, 0).unwrap(),
        };
        // Two samples: one old, one recent.
        for snapshot in [quota(100, 1_000), quota(200, 800_000)] {
            repository
                .commit(SourceTransaction {
                    quotas: vec![snapshot],
                    ..SourceTransaction::new(SourceApp::ClaudeCode, "claude", Utc::now())
                })
                .unwrap();
        }
        assert_eq!(
            repository
                .quota_samples(Utc.timestamp_opt(0, 0).unwrap())
                .unwrap()
                .len(),
            2
        );

        let pruned = repository
            .prune_quota_samples(Utc.timestamp_opt(500_000, 0).unwrap())
            .unwrap();
        assert_eq!(pruned, 1);
        let remaining = repository
            .quota_samples(Utc.timestamp_opt(0, 0).unwrap())
            .unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].used_percent_tenths, 200);
    }

    #[test]
    fn a_version_one_store_upgrades_to_the_current_shape() {
        // Open a fresh store, strip it back to the version 1 shape — no
        // sample table, no `source_ref` column — and stamp it accordingly.
        // Reopening must carry it forward without losing what it held.
        let directory = std::env::temp_dir().join(format!("tokens-mig-{}", std::process::id()));
        let path = directory.join("usage.sqlite3");
        let _ = std::fs::remove_dir_all(&directory);

        let mut dashboard = record("e1", 1, 10);
        dashboard.provenance.source_ref = Some("dashboard-usage-events".to_string());

        {
            // Write first, then strip: the row has to predate the column for
            // the backfill to be the thing under test.
            let repository = SqliteUsageRepository::open(&path).unwrap();
            repository.commit(usage(vec![dashboard])).unwrap();
            repository
                .with_connection(|connection| {
                    connection.execute_batch(
                        "DROP TABLE quota_samples;
                         DROP INDEX IF EXISTS records_by_dataset_time;
                         ALTER TABLE records DROP COLUMN source_ref;
                         UPDATE meta SET value = '1' WHERE key = 'schema_version';",
                    )
                })
                .unwrap();
        }

        let reopened = SqliteUsageRepository::open(&path).unwrap();
        // The store still reads, and the sample table exists again.
        assert_eq!(reopened.count().unwrap(), 1);
        let samples = reopened
            .quota_samples(Utc.timestamp_opt(0, 0).unwrap())
            .unwrap();
        assert_eq!(samples.len(), 0);

        // And the new column was filled from the payload each row already
        // carried, so a scope naming a dataset can find rows written before
        // the column existed.
        let backfilled: Option<String> = reopened
            .with_connection(|connection| {
                connection.query_row("SELECT source_ref FROM records", [], |row| row.get(0))
            })
            .unwrap();
        assert_eq!(backfilled.as_deref(), Some("dashboard-usage-events"));

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_recent_burst_turns_the_snapshot_pace_red() {
        let repository = store();
        let reset = Utc::now() + chrono::Duration::minutes(60);
        // Seed a gentle window-open average: 100 tenths consumed over 4
        // elapsed hours — well green.
        let seed = UsageQuota {
            source_app: SourceApp::ClaudeCode,
            label: None,
            window_minutes: 300,
            used_percent_tenths: 100,
            resets_at: Some(reset),
            observed_at: Utc::now() - chrono::Duration::minutes(30),
        };
        repository
            .commit(SourceTransaction {
                quotas: vec![seed.clone()],
                ..SourceTransaction::new(SourceApp::ClaudeCode, "claude", Utc::now())
            })
            .unwrap();

        // Then the whole of the last 30 minutes is consumed in a burst.
        let burst = UsageQuota {
            used_percent_tenths: 500,
            observed_at: Utc::now(),
            ..seed.clone()
        };
        repository
            .commit(SourceTransaction {
                quotas: vec![burst],
                ..SourceTransaction::new(SourceApp::ClaudeCode, "claude", Utc::now())
            })
            .unwrap();

        let snapshot = repository
            .snapshot(&SummaryQuery::default(), &RecentQuery::default())
            .unwrap();
        let pace = snapshot
            .pace
            .iter()
            .find(|each| each.source_app == SourceApp::ClaudeCode)
            .expect("the live quota yields a pace row");
        // Trailing: 500 remaining * 30 / 400 = 37 min vs T = 60 → red.
        assert_eq!(pace.state, crate::domain::PaceState::Red);
        assert!(pace.shortfall_minutes.unwrap() > 0);
    }

    #[test]
    fn no_conversation_content_reaches_the_store() {
        let repository = store();
        repository.commit(usage(vec![record("e1", 1, 10)])).unwrap();

        let payloads: Vec<String> = repository
            .with_connection(|connection| {
                let mut statement = connection.prepare("SELECT payload FROM records")?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .unwrap();

        // The record shape has no field that could carry a prompt; this holds
        // the line if one is ever added without thinking it through.
        for payload in payloads {
            let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
            let object = value.as_object().unwrap();
            for forbidden in ["prompt", "response", "content", "text", "path"] {
                assert!(
                    !object.contains_key(forbidden),
                    "stored a {forbidden} field"
                );
            }
        }
    }
}
