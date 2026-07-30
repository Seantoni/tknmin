# Automatic Refresh Architecture Plan

Status: proposed implementation plan; no implementation has started.

## 1. Objective

Make token, cost, event, quota, alert, dashboard, mini-window, and menu-bar data
update automatically and quickly without depending on a manual refresh.

The target behavior is:

- Local Codex and Claude Code changes appear within about 500 ms after a
  completed source write under normal conditions, and within 2 seconds at p99.
- Cursor activity is detected locally immediately, while authoritative token and
  billed-cost data is synchronized as soon as Cursor publishes it.
- The interface and menu bar update within about 100 ms after a committed import.
- Missed filesystem events repair themselves within 30–60 seconds.
- Restarting the app shows the last committed data immediately, before catch-up
  work finishes.
- Failures retain the last-known-good values and clearly mark them stale or
  offline. They never silently erase a source.

## 2. Architecture decision: one refresh owner

### Decision

`src-tauri/src/refresh.rs` will be the **only production owner of refresh and
scan orchestration**.

This is the unique source of truth for:

- When any source should be checked
- Which source or data category should be checked
- Filesystem watcher registration
- Debounce and maximum-delay policy
- Timers and reconciliation schedules
- Cursor activity-triggered retries and network backoff
- Per-source single-flight execution
- Trigger coalescing and “dirty again” behavior
- Checkpoint selection and recovery decisions
- Refresh generations and stale-result rejection
- Source health and last-success/last-error state transitions
- Transaction boundaries
- Repository revision increments
- Post-commit publication to React, the mini window, menu bar, and alerts

There must be no second timer, polling loop, direct scan call, quota refresh
loop, or direct refresh event publisher elsewhere in the application.

### Important boundary

One owner does **not** mean putting all source-format code into one giant file.
That would make `refresh.rs` a fragile parser monolith.

The boundary is:

- `refresh.rs` owns **when, whether, and in what order** work runs.
- `adapters/*.rs` own **how one explicitly requested source delta is read and
  parsed**.
- `repository/*` owns **how a transaction is persisted and queried**.
- React owns **how a committed snapshot is rendered**.

Adapters must not:

- Start threads, timers, watchers, or retries
- Decide their polling cadence
- Call themselves on focus, startup, or file changes
- Emit Tauri refresh/data events
- Update global refresh state
- Directly refresh the menu bar or alerts
- Silently retain a private competing checkpoint

Calling an adapter from `refresh.rs` is refresh orchestration. The adapter's
JSONL, SQLite, or HTTP parsing is source implementation, not a second refresh
owner.

### Enforce the boundary in types

Do not rely only on code-review discipline. Split repository capabilities:

- `UsageReader` exposes snapshots, summaries, recent events, counts, quotas,
  health, and revision.
- `UsageWriter` exposes transactional delta application, source-generation
  replacement, checkpoint commits, quota merging, health mutation, and revision
  advancement.

Commands, menu-bar rendering, alerts, and query state receive only
`UsageReader`. `refresh.rs` receives the sole production `UsageWriter`. Remove
the broadly callable `AppState::set_quotas`, repository `clear`, and direct
`insert_batch` capabilities from general application state.

This prevents a future command or UI helper from accidentally creating another
refresh writer.

### Manual “sync now”

A manual recovery action may remain, but it will only submit
`RefreshTrigger::Manual` to the coordinator in `refresh.rs`. It must not contain
or call a separate scanning path. The app must remain correct if the user never
presses it.

## 3. Target control flow

```text
startup / file event / Cursor activity / focus / wake / network / timer / manual
                                      |
                                      v
                    src-tauri/src/refresh.rs
          policy -> queue -> debounce -> single-flight worker
                                      |
                    explicit per-source delta request
                                      |
                                      v
                          source adapter
                   read/parse requested delta only
                                      |
                                      v
                   one repository transaction
          upsert records + quotas + checkpoints + health
                     increment monotonic revision
                                      |
                              commit succeeds
                                      |
                                      v
                    refresh.rs publishes revision
              React + mini + menu bar + alerts update
```

Periodic reconciliation enters through the same coordinator. It is a safety
net, not a separate implementation.

## 4. Non-negotiable invariants

1. Exactly one in-flight usage job per source.
2. Exactly one in-flight quota job per network account endpoint.
3. A slow Cursor request never blocks Codex or Claude Code.
4. Every visible data event refers to an already committed repository revision.
5. Revisions increase monotonically.
6. Older job generations cannot overwrite newer results.
7. Mutable source events are upserted, not permanently skipped.
8. A failed source attempt preserves its last-known-good data.
9. Source freshness and app-sync freshness are stored separately.
10. Watchers provide speed; reconciliation provides correctness.
11. No raw prompt or response content is persisted.
12. No source token, cookie, or credential is included in logs, events, or
    checkpoints.
13. Cursor connect/disconnect swaps only Cursor data, and only after a successful
    replacement dataset is ready.
14. The final application contains no legacy full-refresh owner or old refresh
    event path.

## 5. Step-by-step implementation plan

### Step 0 — Freeze the behavioral contract

Before changing runtime behavior:

- Add tests that capture current normalized token, cost, quota, and dedupe
  results.
- Record representative Codex, Claude Code, local Cursor, and Cursor dashboard
  fixtures.
- Define the target freshness and recovery metrics from this document as
  acceptance tests where practical.
- Document which source fields are authoritative, mutable, provisional, or
  unavailable.

Gate:

- Existing aggregate and adapter fixtures pass unchanged.
- There is a fixture for a repeated Claude streaming `message.id`.
- There is a fixture for a corrected Cursor billed-cost event.

### Step 1 — Define the coordinator contract in `refresh.rs`

Add the single orchestration vocabulary to `refresh.rs`:

- `RefreshCoordinator`
- `RefreshHandle`
- `RefreshTrigger`
- `RefreshCategory`
- `SourceJobState`
- `RefreshPolicy`
- `SourceSyncHealth`
- `RepositoryRevision`

The trigger set should cover:

- Startup
- Local source changed
- Claude quota cache changed
- Cursor local activity
- Cursor connection changed
- Scheduled quota check
- Reconciliation
- Window focus/show
- macOS wake
- Network recovery
- Manual recovery

`RefreshPolicy` is the only place containing refresh timing, debounce, retry,
backoff, and reconciliation values.

Gate:

- Other modules can submit a trigger and query status through `RefreshHandle`.
- Other modules cannot call adapter scans or own refresh timers.
- The coordinator facade is the only production code given repository write
  capability.

### Step 2 — Add persistent repository storage

Replace the in-memory-only production repository with app-owned SQLite in WAL
mode. Keep the in-memory implementation for focused tests if useful.

Persist:

- Normalized usage records
- Source event identity and content/version hash
- Quota snapshots
- Source checkpoints
- Source health and last error
- Last attempt time
- Last successful app sync time
- Source-observed time
- Repository revision
- Schema and normalization versions

Use versioned migrations and a transaction for every committed source batch.
Move Cursor's app-owned last-good quota cache into this generic repository.
Credentials remain in Keychain, but an adapter must not maintain a competing
private quota-state store.

Gate:

- Relaunch immediately returns the last committed dashboard.
- A failed transaction changes neither data nor revision.
- No raw conversation content is stored.

### Step 3 — Replace insert-or-skip with source-aware upsert

Define canonical identity separately from content:

- Codex: stable rollout identity plus token-event ordinal
- Claude Code: source plus `message.id`
- Cursor local: stable bubble/request/usage identity
- Cursor dashboard: immutable server identifier if available; otherwise a
  documented invariant identity plus overlap-window reconciliation

Store a content/version hash so a later snapshot with the same identity replaces
the earlier record when meaningful fields changed.

For Cursor dashboard data without a trustworthy immutable ID, refresh a bounded
overlap window transactionally and replace that source/time slice. A corrected
cost must not coexist with the obsolete cost.

Gate:

- An interim Claude snapshot is replaced by its final snapshot.
- Replaying identical content is a no-op.
- A corrected Cursor cost updates one record rather than creating two.
- Inserted, updated, unchanged, and deleted counts are reported separately.

### Step 4 — Route every existing refresh entry through the coordinator

Wire the coordinator before adding faster triggers:

- Startup submits `Startup`.
- The current manual command submits `Manual`.
- Cursor connect/disconnect submits `CursorConnectionChanged`.
- Quota deadlines submit `ScheduledQuota`.
- Window focus/show submits a lightweight catch-up trigger.
- Wake and network recovery submit reconciliation triggers.

At this stage, adapter work may still use coarse reads internally, but there
must already be only one owner, one queue, and one publication path.

Gate:

- Concurrent startup/manual/connection triggers coalesce safely.
- No stale generation can overwrite a newer Cursor connection mode.
- There are no direct `refresh_all` or `refresh_quotas` calls outside
  `refresh.rs`.

### Step 5 — Implement incremental Codex ingestion

Watch:

- `~/.codex/sessions/`
- `~/.codex/archived_sessions/`

Checkpoint per rollout:

- Stable file identity
- Path/source reference
- Committed byte offset
- Incomplete trailing bytes
- Rollout ID
- Provider
- Current model
- Current working-directory-derived project
- Next token-event ordinal
- Last size and modification time

Behavior:

- Read only bytes after the committed offset.
- Parse only through the final complete newline.
- Keep an incomplete final line for the next attempt.
- Reset safely when a file shrinks or its identity changes.
- Treat moves into the archive as the same rollout, not a duplicate source.
- Extract Codex quota information from the same appended token events.

Gate:

- Appended events appear without rereading historical rollouts.
- Partial final lines never produce invalid or duplicate records.
- Archive moves do not double-count.
- Model switches preserve the correct per-event model.

### Step 6 — Implement incremental Claude Code ingestion

Watch:

- `~/.claude/projects/` recursively
- The parent directory of `~/.claude.json`, because the file may be replaced
  atomically

Checkpoint transcript files using the same offset/incomplete-line rules as
Codex.

Behavior:

- Tail only appended transcript bytes.
- Upsert every assistant snapshot by `message.id`.
- Allow a later streaming snapshot to replace token fields from an earlier one.
- Include newly created nested subagent directories and files.
- Parse `~/.claude.json` only after a stable read.
- Publish quota changes immediately when Claude Code updates its cache.
- Never call Anthropic's undocumented usage endpoint as a polling shortcut.

Gate:

- Streaming updates converge on the final token counts.
- New nested subagent logs are discovered without restart.
- Atomic replacement of `.claude.json` is handled.
- Claude quota freshness reflects the source's `fetchedAtMs`, not merely the
  app's read time.

### Step 7 — Implement Cursor local activity and incremental local mode

Watch the Cursor global-storage directory, including:

- `state.vscdb`
- `state.vscdb-wal`
- Relevant file replacement/rename events

Local-mode behavior:

- Debounce DB/WAL bursts.
- Open a consistent read-only view.
- Query only candidate rows changed since the persisted watermark where a
  reliable watermark is available.
- If exact row-level change detection is unavailable, bound the local overlap
  query and reconcile it transactionally.
- Retry a temporarily inconsistent live view instead of immediately committing
  the explicitly staler immutable fallback.

Connected-dashboard behavior:

- Treat local DB/WAL activity as a signal that authoritative server data may
  soon change.
- Do not treat recent local zero token counts as authoritative billed usage.

Gate:

- Normal Cursor write bursts result in one coalesced job.
- A torn/inconsistent read cannot overwrite newer good data.
- Local mode can repair updated bubbles rather than retaining obsolete unknown
  records.

### Step 8 — Split Cursor backfill from fast delta synchronization

Keep two coordinator-owned operations:

1. Historical backfill/reconciliation of the authoritative rolling 30-day
   window
2. Fast recent-window synchronization after detected activity

Behavior:

- Persist the last authoritative server watermark.
- Query a deliberate overlap before the watermark to catch late/corrected
  events.
- Reuse one HTTP client rather than constructing one per page.
- Respect `429`, `Retry-After`, exponential backoff, and jitter.
- Stop activity-triggered retries once a newer authoritative revision arrives
  or the policy's retry window is exhausted.
- Periodically reconcile the entire rolling window to catch corrections,
  deletions, and late publication.

Cursor's upstream publication time remains outside this app's control. The UI
must distinguish “activity detected” from “billing data published.”

Gate:

- Normal active synchronization does not download all 30 days.
- Full reconciliation converges to the same dataset as a clean backfill.
- Offline and rate-limited operation retains the last-good billed cost.
- A connection-token change cannot mix local and dashboard Cursor rows.

### Step 9 — Make quota synchronization source-keyed

Store and merge quota snapshots by:

```text
(source, optional pool label, window length)
```

Rules:

- Apply a snapshot only if its source `observed_at` is newer.
- A failed attempt updates health/error metadata, not the quota value.
- Expired windows are removed intentionally in a successful source transaction.
- Cursor quota work has its own network lane.
- Codex and Claude local quota changes do not wait behind Cursor.
- Alert evaluation uses the same committed quota revision.
- Stale quota data can trigger a stale-data warning; it must not silently imply
  that the user is safe.

Gate:

- One failed source does not remove another source or its own last-good value.
- An older overlapping task cannot replace a newer snapshot.
- A temporary empty/error response cannot reset alert deduplication state.

### Step 10 — Add watchers and reconciliation inside `refresh.rs`

All watcher registration and callback-to-trigger translation live in
`refresh.rs`.

Initial policy values to validate under real workloads:

- Local quiet debounce: 250–500 ms
- Maximum local burst delay: 2 seconds
- Cheap metadata reconciliation: every 30–60 seconds
- Full/source-window reconciliation: every 10–15 minutes
- Immediate cheap reconciliation after wake, focus/show, and network recovery
- Cursor activity retries: bounded adaptive sequence controlled by backoff and
  provider responses

Raw filesystem callbacks must only enqueue dirty-source triggers. They must not
read, parse, write the repository, or emit UI events directly.

Gate:

- Burst writes are coalesced.
- A source becoming dirty while running causes exactly one follow-up job.
- A deliberately dropped watcher event is repaired by reconciliation.
- Sleep/wake does not require a manual refresh.

### Step 11 — Publish one revisioned state contract

After a transaction commits, `refresh.rs` publishes:

```text
data-changed {
  revision,
  affectedSources,
  affectedCategories,
  inserted,
  updated,
  deleted,
  appSyncedAt
}
```

Expose one atomic `dashboard_snapshot(filter)` query returning:

- Unfiltered overview
- Selected summary
- Recent events
- Record count
- Quotas
- Source metadata
- Per-source sync health
- Source-observed timestamps
- App-sync timestamps
- Repository revision

Subscription handshake:

1. React subscribes to `data-changed`.
2. React fetches `dashboard_snapshot`.
3. React ignores events at or below the snapshot revision.
4. React fetches again only when it receives a higher revision.

The menu bar and alerts update from the same committed revision. No component
publishes an alternative refresh event.

Gate:

- Dashboard fields cannot come from mixed revisions.
- An event emitted during initial load cannot be lost.
- Main and mini windows converge on the same revision.
- The menu bar does not recompute usage merely because an unchanged quota poll
  ran.

### Step 12 — Replace frontend refresh behavior

Remove frontend ownership of refresh cadence and scanning:

- Remove direct `refresh_logs` invocation from the dashboard hook.
- Remove `usage_sources` / `fetchUsageSources` from the render path. It currently
  appears read-only but calls adapter discovery and traverses live sources.
  Static capabilities and coordinator-cached source health belong in the atomic
  snapshot.
- Remove `usage-imported` and `quotas-updated` subscriptions.
- Subscribe to the single revisioned state contract.
- Fetch one snapshot rather than six independent commands.
- Do not add React timers for usage or quota polling.
- Focus/show may submit a catch-up trigger, but it must not perform a scan.
- Keep stale-request and unmount guards.
- If a manual recovery button remains, label it “sync now” and have it submit
  one coordinator trigger.

Display per source:

- Watching / syncing / current / stale / offline / error
- “App synced … ago”
- “Source reported … ago”
- Last non-secret error
- Cursor “activity detected; awaiting billing data” when applicable

Use enough decimal precision that a real sub-cent cost change is visible without
misrepresenting the source's precision.

Gate:

- No frontend timer or direct scanning command exists.
- Filter changes during a sync cannot reload data for the previous filter.
- Initial quota/data events cannot be dropped while the snapshot is loading.
- A failed sync is visible while last-good values remain on screen.

### Step 13 — Remove all legacy refresh paths in the same cutover

The final migration must delete, not deprecate indefinitely:

- `refresh_all`
- `refresh_quotas`
- The sleep-first quota loop
- Startup's direct background full import
- Direct refresh calls from commands
- Direct state/menu/alert mutation after command-owned scans
- `usage-imported`
- `quotas-updated`
- Frontend `refreshLogs` API and hook path
- Frontend `fetchUsageSources` and command-owned live adapter discovery
- Any second full-scan timer or watcher
- Any adapter-owned retry or polling schedule
- Documentation describing manual refresh as the normal data path

Do not ship the new coordinator alongside the old loops “temporarily.” The
cutover is complete only when every trigger enters `RefreshCoordinator`.

Gate:

- The no-leftover audit in section 7 passes.
- Removing the manual button entirely would not change automatic correctness.

### Step 14 — Performance, correctness, and recovery validation

Test:

- Partial JSONL line followed by completion
- File append, creation, rename, archive move, truncate, and replacement
- New recursive Claude subagent directory
- Claude interim-to-final message replacement
- Cursor DB/WAL burst coalescing
- Cursor late, duplicate, corrected, and deleted server events
- Cursor `429`, timeout, offline, and token replacement
- Per-source single-flight and dirty-again behavior
- Concurrent startup, focus, manual, quota, and connection triggers
- Stale generation rejection
- Last-good quota preservation
- Alert deduplication during errors and recovery
- Event-after-commit ordering
- Monotonic revisions
- Frontend subscription/snapshot race
- macOS sleep/wake and network recovery
- Dropped watcher event repaired by reconciliation
- Immediate persisted startup

Measure:

- Source write to committed revision
- Committed revision to visible UI/menu update
- CPU and disk usage while idle
- Bytes reread per local update
- Cursor requests per active and idle hour
- Reconciliation convergence time

Release gate:

- Local source p95 and p99 targets are met.
- Cursor behavior respects provider backoff.
- A full day of normal use needs no manual recovery.
- No token or cost duplication appears after corrections or reconnects.

## 6. File ownership after the migration

### `src-tauri/src/refresh.rs`

Owns all refresh policy, triggers, scheduling, watchers, workers, coordination,
health transitions, transaction publication, revisions, and post-commit
notifications.

### `src-tauri/src/lib.rs`

Creates and starts one coordinator. It contains no refresh duration, polling
loop, scan thread, direct adapter call, or refresh event emission.

### `src-tauri/src/commands.rs`

Queries snapshots/status and submits coordinator triggers. It never scans,
rebuilds, clears, sets quotas, refreshes the menu, evaluates post-refresh alerts,
or emits data-change events directly.

### `src-tauri/src/adapters/*.rs`

Implement source-specific delta reads and parsing only. No cadence, watchers,
global state transitions, or publication.

### `src-tauri/src/repository/*`

Implements durable transactions, upserts, checkpoints, revisions, and atomic
snapshot queries. It does not decide when synchronization runs.

### `src-tauri/src/state.rs`

Holds the repository and coordinator handle. It does not maintain a competing
quota vector or refresh timer state.

### `src-tauri/src/menubar.rs` and `src-tauri/src/alerts.rs`

Render/evaluate an already committed revision when instructed by the
coordinator. They do not initiate scans or polling.

### React hooks and API modules

Subscribe to committed revisions, fetch snapshots, render health, and optionally
submit a manual recovery trigger. They contain no scan cadence or data-source
logic.

## 7. No-leftover refresh audit

Run this audit before the redesign is considered complete.

### Required code-search results

The following legacy names must return no production matches:

```sh
rg -n "refresh_all|refresh_quotas|usage-imported|quotas-updated|QUOTA_POLL_INTERVAL" \
  src src-tauri/src
```

Expected result: no matches.

Direct refresh entry points must exist only in `refresh.rs`:

```sh
rg -n "std::thread::sleep|setInterval|setTimeout|recommended_watcher|watcher" \
  src src-tauri/src
```

Every result must be reviewed. Refresh-related scheduling or watching is allowed
only in `src-tauri/src/refresh.rs`. Unrelated timers must be explicitly
documented.

Direct adapter synchronization calls must be absent outside the coordinator and
adapter tests:

```sh
rg -n "refresh_logs|adapter.*discover|adapter.*read|adapter.*parse" \
  src src-tauri/src
```

Expected production behavior:

- No frontend `refresh_logs`
- No command-owned adapter scan
- No startup-owned adapter scan
- No menu-bar-owned scan
- No alert-owned scan
- No adapter-owned schedule
- No render/query path that calls adapter discovery

Writer capability must also be confined:

```sh
rg -n "set_quotas|repository\\(\\)\\.clear|insert_batch" src-tauri/src
```

Expected production behavior:

- No write calls in `lib.rs`, `commands.rs`, adapters, menu bar, or alerts
- Repository implementation and tests may contain storage primitives
- The production writer capability is held and invoked only by `refresh.rs`

### Runtime assertions

- Log a non-secret coordinator job ID, source, trigger category, generation,
  start time, completion time, and result counts.
- In debug/test builds, assert one active job per source/category.
- Assert revision publication occurs only after commit.
- Assert a result generation is current before applying it.
- Assert every automatic trigger is accepted through the coordinator queue.

### Documentation audit

Update `README.md`, `OVERVIEW.md`, and `PLAN.md` so they describe:

- Automatic incremental synchronization as the normal path
- Manual sync only as recovery/diagnostics
- Source-specific freshness and upstream limitations
- Persistent last-known-good storage
- The single-owner rule in `refresh.rs`

## 8. Recommended delivery sequence

Use four implementation milestones:

1. **Correctness foundation** — persistent repository, upserts, checkpoints,
   revisions, and coordinator contract.
2. **Single-owner cutover** — route all existing triggers through `refresh.rs`
   and delete old loops/events.
3. **Low-latency sources** — Codex/Claude watchers and incremental parsing,
   Cursor activity detection and delta synchronization.
4. **Trust and hardening** — atomic frontend snapshots, health/freshness UI,
   reconciliation, wake/network recovery, load tests, and no-leftover audit.

Each milestone must preserve one-owner semantics. No milestone may introduce a
second refresh loop as an interim shortcut.

Shadow comparison is allowed during development and tests, but it must be
invoked by the coordinator and remain non-authoritative. No production release
may contain two active refresh owners. Full reconciliation remains permanently
available as a coordinator-owned repair operation; it is not a legacy manual
refresh path.

## 9. Final definition of done

The redesign is complete only when:

- `refresh.rs` is the sole refresh/scan orchestrator.
- No other production file owns refresh cadence, watchers, retries, scan
  execution, refresh-state mutation, or data-change publication.
- Adapters only perform work explicitly requested by the coordinator.
- Tokens, billed cost, quotas, dashboard, mini window, menu bar, and alerts
  update without user action.
- The app starts from persisted last-good data and catches up automatically.
- Mutable source events converge through upserts.
- Failures preserve values and expose staleness.
- Reconciliation repairs missed events.
- Cursor upstream delay is visible rather than hidden.
- The legacy refresh paths and event names are deleted.
- The no-leftover audit passes.
