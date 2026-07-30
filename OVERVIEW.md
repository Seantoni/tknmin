# Tokens — App Overview

**Tokens** is a macOS desktop app that answers one question: *how much of my AI
allowance have I burned, and how much is left?* It watches the three agentic
coding tools on this Mac — **Cursor**, **Claude Code**, and **Codex** — reads
their local logs, normalizes everything into one canonical format, and shows
the result in a dashboard window plus an always-on menu bar indicator.

It goes beyond historical totals: each source's *live allowance window* (the
5-hour Claude session, the 7-day week, the Cursor billing month) is kept
current automatically, and when usage crosses a user-set line the app fires a
macOS notification and offers to generate a **handoff brief** so work can move
to another tool before the well runs dry.

Nothing here has to be asked for. The app watches the source files, reads only
what changed, and updates itself; there is a "sync now" button, but it exists
for recovery and diagnostics, and removing it entirely would not change whether
the numbers stay correct.

Everything is local. There are no accounts, no cloud sync, and no telemetry.
The only network calls are to Cursor's own APIs, using the user's own
credentials, and only for Cursor data.

---

## What it does, feature by feature

### Dashboard (main window)

A dark, single-window UI reporting on a **rolling 30-day window** — never
all-time grand totals:

- **Stat bar** — total tokens, input/output/cached/reasoning splits, and cost
  when a source reports it.
- **Source strip** — one card per tool with its share of usage; clicking a
  card filters the whole dashboard to that source.
- **Model breakdown** — usage grouped by model name across all sources.
- **Event list** — the most recent individual usage records, newest first.
- **Quota chips in the header** — e.g. `claude code · 49% left this session`,
  one per source, showing whichever allowance window binds first.

Empty states distinguish "nothing imported yet" from "nothing matched the
filter", and the footer reports how many records were excluded from
date-filtered views because their timestamps couldn't be resolved.

### Menu bar indicator

A status item that shows the compact token total plus one allowance chip per
source: `302K · claude 49% · codex 63%`. When a source meters several windows
at once (Claude runs a 5-hour session inside a 7-day week), the bar shows only
**the window that binds first** — the number that decides whether you can keep
working. The dropdown menu lists every window with its reset time, plus totals
and per-source breakdowns. It reads the same repository as the window, so the
two can never disagree.

### Allowance (quota) tracking

A quota is not a locally reconstructed token total — it is **the source's own
statement** of how much of an allowance window is consumed, and the freshest
snapshot wins:

| Source | Where the quota comes from |
| --- | --- |
| Claude Code | `~/.claude.json` → `cachedUsageUtilization` (the cache behind Claude's own `/usage` command): 5-hour session, 7-day week, and per-model weekly caps (Opus/Sonnet) where the plan has them |
| Codex | `rate_limits` stamped on `token_count` events inside session rollouts; the freshest event across the 10 most recently modified files |
| Cursor | `GetCurrentPeriodUsage` on `api2.cursor.sh` (the same endpoint Cursor's billing UI uses), authenticated with the access token from Cursor's local state DB |

Allowances have their own **lane** in the coordinator, checked on a ~60-second
deadline and immediately whenever a source rewrites its own cache — Claude Code
does that the moment it learns a new number, so the app publishes it without
waiting for the deadline. The lane is separate from usage ingestion so a slow
Cursor request can never delay Codex or Claude Code.

**Freshness is stated, not implied.** Every source carries two clocks, kept
apart on purpose:

- *app synced* — when this app last read the source
- *source reported* — when the source says its data was true

They routinely disagree. Cursor publishes billed usage well after the activity
that caused it, so the interface says "activity detected; awaiting billing
data" rather than implying the allowance is untouched. A failed read never
erases a value: the last committed snapshot stays on screen, marked stale or
offline, with the reason beside it.

### Threshold alerts

Per-source, per-user thresholds (Settings screen), in two flavors:

- **Remaining percent** — warn when a window drops to, say, 25% left.
- **Minutes until reset** — warn when a window resets within N minutes.

When a threshold crosses, the app:

1. Fires a **macOS notification** with two action buttons — *Generate handoff
   MD file* and *Continue* (via `notify-rust`, because Tauri's notification
   plugin doesn't forward action buttons on macOS).
2. Shows an **in-app banner** with the same two actions.

*Continue* snoozes the alert until that window resets. An in-process ledger
dedupes alerts so each crossing notifies once per app run, and stale entries
are pruned when windows reset or usage recovers.

### The handoff flow

The signature workflow: when Claude (or Cursor, or Codex) is about to run out,
**Create handoff** copies a fixed prompt to the clipboard. Paste it into the
still-alive agent and it writes `.handoff/HANDOFF.md` — a structured brief
(goal, done so far, current state, next actions, do-nots, open questions) so
work can resume in a different tool. The prompt text lives in the Rust domain
(`HANDOFF_PROMPT`) and is served to React through a command, so both sides
share exactly one string.

### Cursor dashboard connection (authoritative data)

Cursor's local logs stopped carrying reliable token counts around January
2026. The fix: in Settings, paste the `WorkosCursorSessionToken` cookie from
cursor.com/dashboard. The app validates it against the real API, stores it in
**macOS Keychain** (never on disk in plaintext), then rebuilds Cursor usage
from the dashboard's `get-filtered-usage-events` endpoint — exact input,
output, cache tokens, model, and **billed cost** for the last 30 days.
Disconnecting wipes the credential and falls back to the incomplete local
bubble data. Connecting or disconnecting clears the store and re-imports
everything.

---

## How the data flows

```text
startup / file event / focus / wake / network / timer / manual
                           |
                           v
           policy -> queue -> debounce -> single-flight worker
                           |
                explicit per-source delta request
                           |
                           v
                 one repository transaction
        records + quotas + checkpoints + health, one revision
                           |
                    commit succeeds
                           |
                           v
           publish revision -> React, mini, menu bar, alerts
```

### One refresh owner

`src-tauri/src/refresh.rs` is the **only** production owner of refresh and scan
orchestration. It decides when any source is checked, registers the filesystem
watchers, holds every debounce and retry value, runs at most one job per source
lane, owns the transaction boundary, and publishes the committed revision. No
other file starts a timer, a watcher, a retry, or a scan.

That rule is enforced in the types rather than by review. The repository is
split into `UsageReader` and `UsageWriter`; commands, the menu bar, the alerts,
and every query path receive only the reader, and the coordinator is handed the
sole production writer when it is built. A future command *cannot* accidentally
become a second refresh path, because the capability to write is not in the
handle it is given.

Adapters are the other half of the boundary: they read and parse exactly the
delta the coordinator asked for, and nothing else. An adapter never starts a
thread, decides a cadence, emits an event, or keeps a private checkpoint.

### Reading only what changed

Each source resumes from a **checkpoint** committed in the same transaction as
the records it accounts for — so a failed write can never make the next run
skip data.

- **Codex and Claude Code** are tailed by byte offset. Only the bytes past the
  last committed offset are read, only complete lines are parsed, and an
  incomplete trailing line is carried in the checkpoint to the next attempt.
  Codex additionally carries the rollout id, current model, working directory,
  and next event ordinal, because the `session_meta` line that established them
  may be megabytes behind.
- **Cursor's dashboard** has no immutable event id, so a fast delta re-reads a
  deliberate overlap *before* its watermark and replaces that time slice
  transactionally. A corrected charge lands on the event it corrects instead of
  appearing as a second charge.
- **Cursor's local database** is gated on size and modification time, so a
  burst of write notifications costs one metadata check.

**Watchers provide speed; reconciliation provides correctness.** A cheap
metadata pass every ~45 seconds catches anything a watcher dropped, and a
fuller pass every ~12 minutes re-reads bounded windows so corrections,
deletions, and late publication converge. Sleep, wake, window focus, and
network recovery all enter through the same queue.

**Each stage still fails independently** — a file that vanished mid-read, a
malformed entry, an unreachable endpoint. A failure commits a health row and
touches no data.

### Where each tool's usage actually lives

- **Cursor** — `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`
  (a VS Code state SQLite DB). Usage bubbles sit in `cursorDiskKV` under
  `bubbleId:<conversationId>:<bubbleId>` keys. Two eras coexist: pre-2026
  assistant bubbles carry real `tokenCount` + a unique `usageUuid` (imported
  as token-bearing records); newer bubbles have zeroed counts, so user turns
  with `modelInfo`/`requestId` become token-*unknown* activity events instead.
  When the dashboard is connected, that replaces local bubbles entirely.
- **Claude Code** — one JSONL transcript per session under
  `~/.claude/projects/<slugified-cwd>/`, walked recursively (subagent
  transcripts nest deeper and their usage appears nowhere else). Usage lives
  in `assistant` lines' `message.usage`. Streaming writes several snapshots of
  the same `message.id`, so only the last snapshot per message counts; the id
  doubles as the dedupe identity. Cache-creation tokens are summed into input
  (they're priced as premium input); cache reads stay separate.
- **Codex** — session rollouts under `~/.codex/sessions/YYYY/MM/DD/` plus
  `~/.codex/archived_sessions/`. `token_count` events carry
  `last_token_usage`; the model comes from `turn_context`, the provider from
  `session_meta`. Files are append-only, so `{rollout_id}:{n}` is a stable
  event identity that survives re-imports and the sessions/archived overlap.

---

## Architecture

### Rust backend layers

Strict one-way layering — each layer may depend only on the ones below it:

| Layer | Location | Responsibility |
| --- | --- | --- |
| Commands | `src-tauri/src/commands.rs` | The only surface React can call; thin, delegates everything |
| Refresh | `src-tauri/src/refresh.rs` | **The single owner of when anything is read or written**: triggers, watchers, policy, lanes, transactions, publication |
| State | `src-tauri/src/state.rs` | What the running app holds: a read-only repository handle, the coordinator handle, options, alert ledger |
| Alerts | `src-tauri/src/alerts.rs` | Alert delivery: dedupe ledger, OS notification, window event. Evaluates a committed revision; never scans |
| Menubar | `src-tauri/src/menubar.rs` | Tray icon title + menu, repainted from a committed revision when the coordinator says so |
| Prefs | `src-tauri/src/prefs.rs` | `options.json` load/save (atomic temp-file + rename) |
| Pipeline | `src-tauri/src/pipeline.rs` | Drafts in, validated records out; rejected drafts reported, never raised |
| Repository | `src-tauri/src/repository/` | Durable SQLite (WAL) transactions, upserts, checkpoints, revisions, atomic snapshots |
| Normalize | `src-tauri/src/normalize/` | **The only producer of validated `UsageRecord`s** |
| Adapters | `src-tauri/src/adapters/` | One per source app, producing the requested delta; never validate/dedupe/store, and never decide *when* |
| Fixtures | `src-tauri/src/fixtures.rs` | Deterministic sample dataset, now used only by tests |
| Domain | `src-tauri/src/domain/` | Shared vocabulary; depends on nothing |

The payoff of the layering: everything downstream of the normalizer
(repository, commands, React) can assume invariants already hold, and one
source's malformed log can never affect the others.

### The Tauri command surface

`dashboard_snapshot` (everything one screen renders, at one revision),
`usage_summary`, `recent_usage`, `usage_record_count`, `source_capabilities`,
`usage_quota`, `sync_now`, `window_focused`, `is_syncing`, `get_options`,
`set_options`, `cursor_connection_status`, `connect_cursor_dashboard`,
`disconnect_cursor_dashboard`, `active_alerts`, `snooze_alert`,
`handoff_prompt`, `test_notification`, `show_mini_window`,
`show_dashboard_window`.

None of them scans a source, writes to the repository, or emits a data-change
event. `sync_now`, `window_focused`, and the two Cursor connection commands
submit a trigger to the coordinator and return.

### Events pushed to React

`data-changed` — the one data event, emitted only **after** a commit returns,
carrying `{ revision, affectedSources, affectedCategories, inserted, updated,
deleted, dataChanged, appSyncedAt }`. Plus `threshold-alerts` and
`threshold-handoff-copied`.

`dataChanged` separates "the numbers moved" from "only the freshness moved", so
a quiet minute stays quiet: the interface refetches either way (ages tick), but
the menu bar does not re-aggregate every record because a quota poll found the
same percentage it found a minute ago.

### The subscription handshake

The order matters, and it is what makes the whole thing race-free:

1. React subscribes to `data-changed` **before** fetching, so an event emitted
   during the initial load cannot be lost.
2. It fetches one `dashboard_snapshot`, which names the revision it was read at.
3. It ignores every event at or below that revision.
4. It fetches again only on a higher one.

### Frontend structure

- `src/domain/usage.ts` — TypeScript types mirroring the Rust domain, changed
  only alongside their Rust counterparts.
- `src/api/*.ts` — the **only** modules that call `invoke`.
- `src/hooks/` — `useUsageDashboard` (one revisioned snapshot + the
  subscription handshake, with stale-request and unmount guards), `useQuotas`
  (the same contract for the compact window), `useOptions`,
  `useThresholdAlerts`. **No hook owns a timer or a scan.**
- `src/components/` — `StatBar`, `SourceStrip`, `SourceHealth`,
  `ModelBreakdown`, `EventList`, `AlertBanner`, `SettingsView`, `AppMenu`,
  `Skeletons`.
- `src/theme.ts` — one hue per source on a near-black background
  (cursor `#7aa2f7`, claude code `#dda06a`, codex `#6cc4a1`); nothing else is
  colored, so color always means data.

---

## Design decisions worth knowing

These are the opinionated choices that make the app trustworthy:

- **Unknown is not zero.** Every token category is a
  `TokenField { value, quality }` pair (`exact` / `estimated` / `partial` /
  `unknown`). A `null` value means "not reported", never zero — so totals can
  mark themselves as lower bounds (`unknownRecords > 0`) instead of silently
  undercounting.
- **Money never becomes a float.** Amounts are integer minor units with a
  currency and exponent (`{ amount_minor, "USD", 6 }` = micro-dollars);
  exponents up to 6 allow per-million-token pricing. All formatting — even in
  the menu bar — is integer arithmetic. Cost totals never mix currencies.
- **Deterministic dedupe.** Dedupe keys are SHA-256 over the source's own
  event ID (or over the event's observable content when no ID exists),
  deliberately **excluding adapter version and import time** — upgrading an
  adapter can't resurrect already-imported records. The record ID itself is
  derived from the dedupe key, so the same source event always resolves to the
  same ID.
- **Reproducible normalization.** Normalization is a pure function (clock
  passed in as a parameter). Zone-less timestamps are interpreted as UTC, not
  the Mac's local zone. An unparsable timestamp doesn't reject a record;
  date-filtered queries skip undated records and report how many were skipped.
- **Honest totals.** Display totals prefer a source-reported total, otherwise
  sum input + output (+ reasoning when reported). Cached input is excluded
  because sources generally report it as a component of input. Every total
  carries the rule that produced it, so the UI never guesses comparability.
- **Quota freshness is shown, not faked.** Each quota snapshot carries
  `observed_at`; quota goes stale within minutes of activity, so the interface
  can show age rather than pretending it's live. Percentages are stored as
  integer tenths (930 = 93.0%), like money.
- **Paths never leak.** `DiscoveredSource.source_ref` is an opaque
  adapter-owned handle; provenance records adapter id/version and that handle
  — never filesystem paths, prompts, or responses.

### Security & privacy

- The Cursor dashboard session token lives only in **macOS Keychain**
  (service `com.josep.tokens`).
- Cursor's local access token is read from `state.vscdb`, held in memory, sent
  only to `api2.cursor.sh`, and never logged or cached.
- The on-disk quota cache contains percentages and reset times only — no
  credentials.
- The app is otherwise fully offline: no analytics, no external calls beyond
  Cursor's own APIs for Cursor data.

### Startup & runtime behavior

1. Load options from the app config dir (`options.json`), apply defaults
   (warn at 25% remaining on every source) if missing.
2. Request notification permission up front so the first threshold crossing
   can notify.
3. Open the persistent store and paint it. **The window shows the last
   committed revision immediately** — catch-up work happens behind it, not in
   front of it, so a relaunch is never a blank screen waiting on a log walk.
4. Start **one** coordinator and submit `Startup`. That is the last push the
   application makes; everything after it is a trigger.

Usage records, quota snapshots, per-source checkpoints, sync health, and the
repository revision all persist to app-owned SQLite in WAL mode
(`usage.sqlite3` in the app data directory), under versioned migrations. A
failed transaction changes neither data nor revision. No prompt or response
content is stored, and no source token, cookie, or credential appears in a log,
an event, or a checkpoint — Cursor's credential stays in Keychain and in
memory. Options persist separately as JSON.

---

## Tech stack

**Desktop shell & backend**

- **Tauri 2** (with the tray-icon feature), `tauri-plugin-opener`,
  `tauri-plugin-notification`
- **Rust** (2021 edition): `serde` / `serde_json`, `chrono` + `chrono-tz`,
  `thiserror`, `sha2` + `hex` (dedupe keys), `rusqlite` 0.40 bundled (reading
  Cursor's `state.vscdb`), `reqwest` 0.13 blocking + rustls (Cursor API),
  `keyring` 4.1 (macOS Keychain), `notify-rust` 4 (notifications with action
  buttons)

**Frontend**

- **React 19** + **TypeScript** 5.8, **Vite** 7, plain CSS (`src/App.css`)
- `@tauri-apps/api` for commands + event subscriptions

**Tooling**

- **pnpm** workspaces, `pnpm tauri dev` / `pnpm tauri build`
- `cargo test` — unit tests throughout the Rust crate (adapters use
  path-injecting constructors and fixture modes so tests never touch real
  logs, the Keychain, or the network)

---

## Infrastructure & packaging

- **Platform:** macOS only (developed on Apple Silicon), dark theme, window
  900×680 (min 620×420), bundle id `com.josep.tokens`.
- **Build outputs:** `pnpm tauri build` produces
  `src-tauri/target/release/bundle/macos/Tokens.app` and a DMG under
  `bundle/dmg/`; a verification copy lives at `dist-app/Tokens.app`.
- **Signing:** the build is unsigned and unnotarized — fine for personal use
  on this Mac; both are required before installing on another Mac.
- **Permissions:** the Tauri capability grants `core:default`,
  `opener:default`, `notification:default` to the main window; macOS
  notification permission is requested at runtime.
- **Files the app writes:** `options.json` in the app config dir, and a
  Cursor quota cache (`cursor-usage-cache.json`) beside it — nothing else.

---

## Project status & history

Development follows the phased plan in [PLAN.md](./PLAN.md): scaffold →
packaging proof → architecture contracts → fake-data MVP → verified MVP →
local-log support. All phases are complete: all three adapters are live,
fixtures survive only in tests, and the dashboard reads real logs. Near-term
direction from the plan: a persistent (SQLite) store behind the existing
repository trait, and code signing/notarization for distribution to other
Macs. Longer-term ideas explicitly deferred so far: automatic log watching,
cloud sync, and iOS.

## Useful commands

```sh
pnpm install
pnpm tauri dev        # development window
pnpm tauri build      # standalone macOS .app + DMG
cd src-tauri && cargo test   # Rust unit tests
```
