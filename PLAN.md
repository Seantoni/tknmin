# Tokens — Mac MVP Plan

## 1. Locked decisions

- Stack: Tauri 2, React, TypeScript, and Rust
- Platform: macOS only for the MVP; iOS is deferred
- Initial user: personal use on this Mac
- Future direction: make the app installable on other Macs
- MVP outcome: a standalone `.app` opens from Finder and displays fake token-usage data
- Planned log sources: Cursor, Claude Code, and Codex
- Refresh behavior: manual import/refresh only
- MVP data fields:
  - Timestamp
  - Source application
  - Provider
  - Model
  - Input tokens
  - Output tokens
  - Optional cost
  - Optional project

Real log parsing, automatic watching, cloud sync, authentication, and iOS are not part of the initial MVP.

## 2. Current machine status

Last updated: July 29, 2026

- Apple Silicon (`arm64`)
- macOS 26.5.2
- Xcode is installed
- Node.js 22.21.1 is installed
- npm 10.9.4 is installed
- pnpm 11.1.3 is installed
- Rust 1.97.1 (`stable-aarch64-apple-darwin`) is installed via rustup
- Cargo is available

### Progress

- [x] Prepare the Mac (Rust installed)
- [x] Phase 1 — Scaffold the application
- [x] Phase 2 — Prove standalone packaging early
- [x] Phase 3 — Establish the architecture
- [x] Phase 4 — Build the fake-data MVP
- [x] Phase 5 — Complete and verify the MVP
- [ ] Phase 6 — Add local-log support

### Phase 1–2 notes

- Project scaffolded as Tauri 2 + React + TypeScript + pnpm
- Product name / window title: `Tokens`
- Bundle identifier: `com.josep.tokens`
- Dev/build commands: `pnpm tauri dev`, `pnpm tauri build`
- Standalone app verified: `dist-app/Tokens.app` opens from Finder, quits, and reopens
- Release also produced a DMG during `pnpm tauri build`

### Phase 3 notes

Rust layers, each depending only on the ones below it: `commands` → `state` → `pipeline` → `repository` → `normalize` → `adapters` → `domain`.

Decisions made while establishing the contracts:

- Every token category is a `TokenField { value, quality }` pair, so an unreported count cannot be read as zero
- Money is `{ amount_minor, currency, minor_unit_exponent }` integer minor units; exponents up to 6 allow per-million-token pricing. Cost totals never mix currencies
- Deduplication keys are SHA-256 over the source event ID when one exists, otherwise over the event's observable content. Adapter version and import time are excluded so upgrading an adapter cannot resurrect imported records
- The record ID is derived from the deduplication key, so the same source event always resolves to the same ID
- Zone-less timestamps are interpreted as UTC, not as this Mac's local zone, because normalization must be reproducible. `AssumedLocal` stays available for a source known to write local time
- An unparsable timestamp does not reject a record; date-filtered queries skip undated records and report how many were skipped
- Display totals prefer a source-reported total, otherwise sum input and output plus reasoning when reported. Cached input is excluded because sources generally report it as a component of input
- Adapters are registered stubs that return `NotImplemented`; `usage_sources` reports `readsLogs: false` until Phase 6 implements each one

Commands exposed: `usage_summary`, `recent_usage`, `usage_record_count`, `usage_sources`. React reaches them only through `src/api/usage.ts`; `src/domain/usage.ts` mirrors the Rust types.

Verified: `cargo test` (48 tests), `cargo clippy --all-targets` clean, `pnpm build` type-checks, and `pnpm tauri dev` launches with the commands registered. The dashboard itself is still Phase 4.

### Phase 4 notes

Fake dataset: 18 records in [src-tauri/src/fixtures.rs](src-tauri/src/fixtures.rs), covering July 20–29, 2026 across all three sources. It is a fixed table with a fixed import timestamp and no clock or filesystem access, so every launch produces byte-identical records, identifiers, and totals. `AppState::with_fake_data` seeds the repository at startup; removing the module and that constructor is all Phase 6 needs to undo.

The dataset deliberately includes the awkward cases real logs will produce, so the interface was built against them rather than retrofitted:

- A record with no provider, and another with neither provider nor model
- A zone-less timestamp, which normalizes as `assumed_utc`
- Estimated rather than exact token counts
- A source-reported total that overrides the local sum
- Records with no cost at all, alongside costs both reported by the source and priced locally

Dashboard: summary cards, breakdowns by source and by model, and a recent-usage table, with loading, empty, and error states. Every figure states its own gaps — a total that omits records says how many, estimated counts are marked `≈`, interpreted timestamps are marked `*`, and unreported values show `—` rather than zero. A banner marks the data as sample data so it cannot be mistaken for real usage.

React files: [src/hooks/useUsageDashboard.ts](src/hooks/useUsageDashboard.ts) (loads all four commands in parallel), [src/format.ts](src/format.ts) (money rounding stays in integer arithmetic), and three components under [src/components/](src/components/).

Verified: `cargo test` (56 tests, 8 new covering fixture determinism, totals, and idempotent seeding), `cargo clippy --all-targets` clean, and `pnpm build` type-checks. The running window was inspected during Phase 5.

### Phase 5 notes

Release build on July 29, 2026 produced both bundles:

```text
src-tauri/target/release/bundle/macos/Tokens.app
src-tauri/target/release/bundle/dmg/Tokens_0.1.0_aarch64.dmg
```

`dist-app/Tokens.app` was refreshed from that build. Bundle identity confirmed: name `Tokens`, identifier `com.josep.tokens`, version `0.1.0`, arm64 Mach-O.

MVP acceptance checklist:

| Check | Result |
| --- | --- |
| Launches from Finder | Passed — opened via `open dist-app/Tokens.app` |
| No terminal or local server | Passed — no Vite process, port 1420 free while running |
| Dashboard shows fake usage | Passed — inspected on screen |
| Totals agree with the records | Passed — on-screen figures cross-checked against the fixture table, plus five automated checks |
| Loading and error states | Partly verified — see below |
| Quits and reopens | Passed — quit cleanly, relaunched |

On-screen figures matched the fixtures exactly: Claude Code input 136,770, Codex 50,500, Cursor 60,900; cost 2.48 USD total from 1.85 (Claude Code) and 0.63 (Cursor); cached input 111,120 with 8 records not reporting it; reasoning 20,550 with 12 not reporting it; the unreported-model row present.

[src-tauri/tests/mvp_acceptance.rs](src-tauri/tests/mvp_acceptance.rs) recomputes the summary cards from the exact rows the table shows, so the "totals agree" check runs on every build rather than depending on someone reading the screen. Total suite: 61 tests.

Not fully verified: the error state was never triggered at runtime. Its Rust contract and the `toAppError` mapping are tested, and the loading state was observed, but no failure was forced through the built app. There is no frontend test runner in the project yet; adding one is the cheapest way to close this.

Known rough edge, not a checklist failure: the default 800×600 window is cramped for the eight-column tables, which rely on horizontal scrolling. Worth raising the default window size before the app is used regularly.

## 3. Prepare the Mac

### Install Rust

Install Rust through the official `rustup` installer:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Choose the default installation, restart the terminal, and verify:

```sh
rustc --version
cargo --version
rustup show
```

### Verify Apple development tools

Xcode is already installed. Confirm its command-line tools remain selected:

```sh
xcode-select -p
xcrun --version
```

If Xcode shows a license or first-launch prompt, open Xcode once and complete it before building Tauri.

### Confirm JavaScript tooling

Use pnpm as the project package manager:

```sh
node --version
pnpm --version
```

Do not install application dependencies globally unless Tauri's current documentation specifically requires it.

## 4. Phase 1 — Scaffold the application

From the empty `tokens` directory:

1. Run the current Tauri project generator.
2. Select:
   - Tauri 2
   - React
   - TypeScript
   - pnpm
3. Use `Tokens` as the product name.
4. Give the application a stable reverse-domain bundle identifier.
5. Start the development application and confirm that its native window opens.

Suggested bundle identifier for personal development:

```text
com.josep.tokens
```

Completion criteria:

- The React interface loads in a Tauri window.
- The project starts with one documented command.
- There are no initial build or type-checking errors.

## 5. Phase 2 — Prove standalone packaging early

Create a release build before adding features.

Completion criteria:

- Tauri produces a macOS `.app`.
- The app opens from Finder without a terminal or development server.
- It can be closed and reopened normally.
- The app name and window title are correct.

Signing and notarization are not required for a personal MVP. They will be needed later for smooth installation on other Macs.

## 6. Phase 3 — Establish the architecture

Keep responsibilities clearly separated:

- **React:** dashboard presentation, filters, user interactions, loading states, and error messages
- **Rust:** Tauri commands, log discovery and reading, filesystem access, source adapters, normalization, validation, and deduplication
- **Source adapters:** one adapter for each supported source—Cursor, Claude Code, and Codex. Each adapter parses source-specific entries into a `UsageRecordDraft`
- **Normalizer:** validates and canonicalizes drafts into the versioned usage-record format consumed by the rest of the application
- **Repository:** expose only the operations currently needed, such as summary, recent records, and batch insert. Start with an in-memory implementation and replace it with SQLite when real importing begins

Use this data flow:

```text
discovery → source adapter → UsageRecordDraft → normalizer/validator
          → repository → Tauri command → React
```

Each normalized usage record should include:

- Normalization version
- Stable internal ID
- Raw source timestamp
- Optional normalized UTC event timestamp
- Timestamp interpretation status when a timezone is missing or assumed
- Source application (`cursor`, `claude_code`, or `codex`)
- Source event ID when available
- Deterministic deduplication key and deduplication-algorithm version
- Provider and model when available
- Input, output, cached-input, and reasoning token counts when available
- Per-field token-count quality (`exact`, `estimated`, `partial`, or `unknown`)
- Reported total tokens when supplied by the source
- Optional computed display total with an explicit calculation rule
- Optional project and session identifiers
- Optional cost, ISO currency, calculation status, and pricing version
- Import timestamp
- Adapter version and non-sensitive source provenance

Provider, model, token categories, project, session, and cost must all be allowed to remain unknown because local logs may expose only partial information. Missing values must never be interpreted as zero.

Store monetary values as decimal values or integer minor units, never binary floating-point values.

Phase 3 establishes contracts, module boundaries, and the in-memory repository only. Real log discovery, parsing, and SQLite persistence remain deferred.

## 7. Phase 4 — Build the fake-data MVP

### Rust side

1. Define the normalized usage record.
2. Create a small deterministic fake dataset.
3. Expose two Tauri commands:
   - Usage summary
   - Recent usage records
4. Return clear errors that React can display.

SQLite is intentionally deferred. It is unnecessary for proving the standalone dashboard and would expand the MVP.

### React side

Build one dashboard containing:

- Total input tokens
- Total output tokens
- Total tokens
- Optional fake estimated cost
- Breakdown by source or model
- Recent usage table
- Loading, empty, and error states

Charts are optional. Summary cards and a table are enough for the MVP.

Completion criteria:

- React obtains all displayed usage data through Tauri commands.
- The interface does not depend on a web server or external API.
- Fake values remain predictable between launches.

## 8. Phase 5 — Complete and verify the MVP

Build the release `.app` again and test it outside the development environment.

MVP acceptance checklist:

- The app launches by double-clicking it in Finder.
- No terminal or local server is required.
- The dashboard displays fake token usage.
- Totals agree with the displayed fake records.
- Loading and error states are understandable.
- The app can quit and reopen without failure.

At this checkpoint, the MVP is complete.

## 9. Phase 6 — Add local-log support

This phase starts after the fake-data MVP is accepted.

### First investigate the sources

For Cursor, Claude Code, and Codex:

1. Locate the relevant log or transcript files on this Mac.
2. Save small sanitized samples for development fixtures.
3. Document which token fields each source actually provides.
4. Confirm whether files are JSON, JSONL, SQLite, or another format.
5. Check whether formats vary by version.

Do not assume that all three sources expose provider, model, cost, or identical token categories.

### Source investigation findings (July 29, 2026)

Investigated on this Mac. Sanitized samples live in [src-tauri/fixtures/samples/](src-tauri/fixtures/samples/) (prompt text, paths, git metadata, Codex `base_instructions`, and `rate_limits` plan info all redacted).

**Claude Code** — richest, cleanest source.

- Location: `~/.claude/projects/<slugified-cwd>/<session-uuid>.jsonl` — one JSONL transcript per session; 90 files across 5 project dirs here.
- Usage lives in `type: "assistant"` lines: `message.model` plus `message.usage.{input_tokens, output_tokens, cache_creation_input_tokens, cache_read_input_tokens, service_tier}`. Line level adds ISO-8601 UTC `timestamp`, `uuid`, `sessionId`, `requestId`, `cwd`, `gitBranch`, `version`.
- Dedup caveat: streaming writes several assistant lines per API message — the same `message.id` repeats with identical usage. Dedupe on `message.id` keeping the last line, otherwise usage is double-counted.
- No cost field. Price locally or leave unreported.
- Format stable across all observed files (version 2.1.x).

**Codex** — rich, with cumulative totals that need a decision.

- Location: `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<uuid>.jsonl`, plus `~/.codex/archived_sessions/`. `~/.codex/session_index.jsonl` maps session id → thread name. (`~/.codex/logs_2.sqlite` is tracing/debug output, not usage data.)
- Every line is `{timestamp, type, payload}`. Key types: `session_meta` (id, timestamp, cwd, `model_provider`, `cli_version`), `turn_context` (`model`, `effort`, `timezone`), and `event_msg` with `payload.type == "token_count"` carrying `info.last_token_usage` (per-turn delta) and `info.total_token_usage` (cumulative), both `{input_tokens, cached_input_tokens, cache_write_input_tokens, output_tokens, reasoning_output_tokens, total_tokens}`, plus `model_context_window`.
- 653 of 752 rollout files (87%) contain `token_count` events; the remainder are empty or renamed stub sessions. The oldest sessions (February 2026) predate the event entirely — the adapter must tolerate its absence.
- Decision deferred to adapter design: one record per turn (from `last_token_usage`, joins to `turn_context.model`) versus one record per session (final `total_token_usage`). Per-turn fits the existing record model better.
- No cost field (`rate_limits` carries plan type, not usage; ignored).

**Cursor** — local storage is a stack, not a transcript; token counts are sparse and stale.

Sources investigated on this Mac (July 29, 2026; Cursor Ultra, `state.vscdb` ≈ 7.5 GB):

| Source | What it holds | Tokens? | Verdict |
|---|---|---|---|
| `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb` → `cursorDiskKV` `bubbleId:<conv>:<bubble>` | Per-message bubbles: `type` (1=user, 2=assistant), `createdAt` ISO, `tokenCount.{input,output}`, `modelInfo` (on user bubbles), `requestId` (on user bubbles), `usageUuid` (when tokens were filled) | **Sometimes** | **Primary** |
| Same DB → `composerData:*` + `ItemTable.composer.composerHeaders` | Session index (578 here): `modelConfig`, `contextUsagePercent`, lines changed | No (context window ≠ billing) | Enrichment only |
| `~/.cursor/ai-tracking/ai-code-tracking.db` → `ai_code_hashes` | `model`, `requestId`, `conversationId`, `timestamp` (authorship) | No | Model enrichment |
| `workspaceStorage/*/state.vscdb` → `aiService.generations` | `{generationUUID, type, unixMs, textDescription}` (~1.2k across 41 workspaces) | No | Weaker event log |
| `~/.cursor/projects/*/agent-transcripts/*.jsonl` (735 files) | `{role, message}` text only | No | Ruled out for usage |
| `cursorDiskKV` `agentKv:blob:*` (~375k rows / 3.3 GB) | Content-addressed blobs; no structured usage schema | No | Ruled out |
| `aiCodeTracking.dailyStats.*` | Tab/composer lines suggested/accepted | No | Ruled out |
| Local plan-quota cache (`planUsage` / `totalPercentUsed`) | — | — | **Absent** (unlike Claude's `cachedUsageUtilization`) |

Bubble coverage on this machine (copied DB for a consistent scan):

- 164,487 `bubbleId` rows; **999** with non-zero `tokenCount` (**0.6%**).
- Every non-zero bubble is dated **2025-12-14 → 2026-01-19**. **Zero** token-bearing bubbles after that — i.e. all recent Agent/composer traffic has `{inputTokens:0, outputTokens:0}`.
- When present, `usageUuid` is unique (997/997) and is the right dedup identity; older rows often lack `modelInfo` on the assistant bubble.
- Current chat shape: model + `requestId` live on **user** bubbles (`type=1`); assistant bubbles (`type=2`) are many-per-turn (tool/thinking `capabilityType` 15/30), usually with empty `requestId` and zero tokens.
- Cursor staff (forum): `tokenCount` is **best-effort** — client tries to backfill after the stream, often fails; dashboard/API is the billing source of truth. Community tools (codeburn, budi, vibe-replay) still read bubbles, but treat zeros as incomplete (codeburn falls back to char/4 estimates — we will not invent counts).

Plan “usage left” is **not** in local logs. Community status-bar tools call `POST https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage` with `cursorAuth/accessToken` from the same DB (`planUsage.totalPercentUsed` / `apiPercentUsed`, spend in cents). That is a network path, outside the local-log adapter contract — same situation Claude had before we found `~/.claude.json`, except Cursor does not appear to cache the reply locally.

**Recommended Cursor adapter (local logs only):**

1. **Discover/read** the single global `state.vscdb` (open read-only; prefer a snapshot copy or `immutable=1` — the live DB is multi-GB and WAL-active while Cursor runs). Do not walk `agentKv`.
2. **Parse `bubbleId:*` keys** (`bubbleId:<conversationId>:<bubbleId>`, key length 82). Unit of record = **one user turn** (`type=1`): timestamp from `createdAt`, model from `modelInfo.modelName`, `source_event_id` = `requestId` when present else the bubble UUID, `session_id` = conversation id from the key. Attach tokens only from a related assistant bubble when `tokenCount` is actually non-zero (or when `usageUuid` is set); otherwise leave every token field `unknown` — never char-count.
3. **Enrichment (optional, same refresh):** `composerData.modelConfig` / headers for session model fallback; `ai_code_hashes` by `conversationId`/`requestId` when the bubble has no model.
4. **Honest UI consequence:** Cursor's token/cost totals will under-count vs reality for everything after ~Jan 2026; the sources strip should still show live event/model activity. Do not pretend zeros are zeros of usage.
5. **Quota requires Cursor's account endpoint:** there is no local cache equivalent to Claude's. Implemented below after the user explicitly required real usage rather than tokenless events.

Not recommended: treating `contextUsagePercent` as plan quota; char/4 estimates; importing every `type=2` tool bubble as a usage event; scanning the 3 GB `agentKv` tree.

**Cross-cutting**

- Timestamps everywhere: Claude Code and Codex use ISO-8601 UTC; Cursor uses `unixMs` epoch milliseconds.
- No source reports cost in a locally parseable form; cost comes from local pricing or stays unreported.
- Stable source event IDs everywhere (Claude `message.id`/`requestId`, Codex rollout UUID + line timestamp, Cursor `generationUUID`), so the existing SHA-256 dedup rule applies unchanged.
- Suggested adapter order: Codex first (structured totals, single provider), then Claude Code (dedup subtlety), then Cursor (unreported tokens).

### Phase 6 progress notes (July 29, 2026)

**Done: the Codex adapter reads real logs; the app no longer shows fake data.**

- `CodexAdapter` implements discovery (sessions tree + archive), reading, and parsing. Each `token_count` event becomes one draft from `last_token_usage`, with the model from the enclosing `turn_context`, provider from `session_meta`, and `{rollout_id}:{n}` as the stable event ID — which also dedupes the sessions/archived overlap automatically.
- A new `refresh` pass runs discover → read → parse → import over every registered adapter, each stage failing independently. `refresh_logs` exposes it as a command; the report carries per-source imported/skipped/failed counts as the plan requires.
- Startup imports in a background thread (the full scan takes ~3s against 837MB of logs), then repaints the menu bar and emits `usage-imported`; the dashboard listens and fills in. The window no longer blocks on log IO.
- The fake dataset no longer seeds the app: `fixtures` and `AppState::with_fake_data` remain for tests only, and the sample-data pill is gone from the interface. The footer now marks each source `live` or `soon` — Cursor and Claude Code stay stub-planned until their adapters land.
- Verified against the real logs: 752 rollout files, 46,881 drafts, 41,688 records stored, 5,193 duplicates skipped (sessions/archived overlap), 0 rejected, 0 failed. `cargo test` 75 tests, `cargo clippy --all-targets` clean, `pnpm build` type-checks, `pnpm tauri build` produces both bundles. `src-tauri/examples/import_logs.rs` reruns the same import from the command line.

**Still to come in Phase 6:** SQLite persistence (imports currently rebuild the in-memory store on every launch), and per-source failure surfacing in the interface beyond the refresh report.

**Cursor adapter (July 29, 2026):** reads the single global `state.vscdb` (`cursorDiskKV` keys `bubbleId:<conversationId>:<bubbleId>`). `read` opens the DB read-only (immutable fallback if the live WAL is torn), extracts only the fields the import needs into compact JSONL, and never loads the multi-GB bubble payloads into the rest of the pipeline. Parse mixes two eras without double-counting: assistant bubbles with real `tokenCount` become token-bearing records (`usageUuid` as event id, model inherited from the preceding user turn); user turns whose following assistants are all zero-token become events with unknown tokens (`requestId` as event id, `modelInfo.modelName`). No char/4 estimates and no `agentKv` scan. First full import: 7,804 Cursor records (alongside Claude 5,607 and Codex 41,688), ~58s for the combined refresh against a 7.5 GB state DB.

**Cursor current-period usage (July 29, 2026):** recent local token counts are genuinely unavailable, so the adapter now calls the Cursor-hosted Connect endpoint used by its billing UI: `POST https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage`. Authentication is the existing `cursorAuth/accessToken` read from `state.vscdb`; it stays in memory, is sent only to `api2.cursor.sh`, and is never logged or persisted. The parser accepts direct/wrapped responses and numeric strings, mapping `autoPercentUsed` to the separately labelled **Cursor Models** pool and `apiPercentUsed` to **Other Models**, with the billing-cycle end as reset; `totalPercentUsed` remains a compatibility fallback only when neither pool is present. Only the non-secret normalized quota vector is atomically cached under `~/.tokens/` for offline fallback. The dashboard renders both pools as distinct `used` / `left` bars, its header and the compact menu-bar title use whichever pool has less remaining, and the tray menu lists both labels and reset times. Live verification returned **2.6% Cursor Models used / 97.4% left** and **13.1% Other Models used / 86.9% left**, both resetting August 23.

**Cursor authoritative token usage (July 29, 2026):** the local bubble DB cannot recover recent token counts, but Cursor's dashboard can. Settings now offers an explicit Cursor Dashboard connection using the browser's `WorkosCursorSessionToken`; the token is validated only against `https://cursor.com/api/dashboard/get-filtered-usage-events`, stored in macOS Keychain (`com.josep.tokens`), never returned to React after submission, and removed on disconnect. Once connected, the adapter replaces local Cursor bubbles with the endpoint's rolling 30-day event stream, paginated at 500 rows per request. Each event carries exact input/output/cache-read/cache-write tokens, model, timestamp, and authoritative `chargedCents`; cache writes join input, cache reads stay in `cached_input`, and fractional cents are retained at four-decimal USD precision. Stable source ids hash the full server event plus a deterministic duplicate occurrence, so refreshes remain idempotent even though Cursor exposes no event id. Rebuilding the in-memory store on connect/disconnect avoids mixing the remote billing events with the local best-effort bubbles.

**Weekly allowance in the dashboard (July 29, 2026):** the Codex `token_count` events also carry `rate_limits` with a weekly window (`used_percent`, `window_minutes: 10080`, `resets_at`). The adapter now exposes the freshest snapshot via a new `SourceAdapter::quota` method — picked by newest event time across the ten most recently modified rollouts, since a session started days ago can still be appending. Quotas ride the refresh pass (`RefreshReport.quotas`), are held in `AppState` outside the repository (account state, not usage data), and reach React through `usage_quota`. The header shows e.g. `codex · 63% left this week`, with reset time and snapshot age in the tooltip. Percentages travel as integer tenths, like money; only the `primary` window is surfaced (`secondary` has been null in every observed file). The menu bar title carries the same number in whole percent beside the token total (`302K · 63%`), and the tray menu adds one informational line with the reset date.

**Rolling 30-day dashboard (July 29, 2026):** the dashboard no longer reports all-time grand totals. Every query it issues — the unfiltered overview, the source-filtered summary, and the recent-events page — carries a `from` bound of `now − 30d`, so tokens, cost, events, model shares, and the sources strip all describe the same rolling window. The header shows a `last 30 days` note; when the store holds records but none inside the window the empty state says so ("No usage in the last 30 days."), which stays distinguishable from an empty store because the all-time record count is still fetched unscoped. The filtered events counter reads `N of M` where both numbers are in-window. The menu bar is unchanged: its title is meant as an all-time-style glance, not a windowed report.

**Claude Code adapter (July 29, 2026):** reads `~/.claude/projects/**/*.jsonl` — session transcripts plus the nested `subagents/agent-*.jsonl` files, which hold Task-tool usage that appears in no parent transcript (verified by cross-referencing message ids). Streaming writes several lines per `message.id`, each a snapshot of the same call, so the parse keeps only the last snapshot per message; the id is globally unique and doubles as the deduplication identity, which also protects against transcripts ever overlapping. Token mapping: `input = input_tokens + cache_creation_input_tokens` (cache writes are uncached input-side work, priced above plain input), `cached_input = cache_read_input_tokens`, `output = output_tokens`; Claude Code reports no total, no reasoning split, and no cost, so display totals follow the `InputPlusOutput` rule. Provider is the constant `anthropic`. `source_ref` is the path relative to the projects dir (nesting keeps `agent-*` names unique); the path never leaves the adapter. First full import: 90 sources, 5,607 records, alongside Codex's 41,688.

**Claude allowance percentages (July 29, 2026):** Anthropic publishes no local quota file and no documented API for subscription usage — the open feature requests (`anthropics/claude-code#34199`, `#56041`, `#27915`) confirm it. Four options were investigated:

1. **The undocumented OAuth endpoint** `GET https://api.anthropic.com/api/oauth/usage`, which is what `/usage` actually calls. Authoritative, but it needs the OAuth token out of the Keychain, spends account rate limit on every poll (the `/usage` command shares its budget with session startup, and over-polling locks out *all* terminals for about an hour), and would make this app a network client instead of a log reader.
2. **Driving `/usage` in a PTY** and scraping ANSI output. Fragile, and it starts a real session, which costs allowance.
3. **Estimating from imported transcripts** — tokens in the last five hours against a guessed ceiling. No credentials needed, but the plan's ceiling is not public, so the number would be invented. Rejected: a fabricated percentage is worse than none.
4. **`~/.claude.json` → `cachedUsageUtilization`** ← chosen. Claude Code calls the endpoint itself and caches the reply locally, refreshing it as you work. Reading that file gives the same server-side figures `/usage` prints, with zero credential handling, zero network traffic, and zero allowance spent — a local-file read, which is the premise of this app.

The cache holds `five_hour` and `seven_day` windows (plus `seven_day_opus`/`seven_day_sonnet`, non-null only on plans with per-model caps), each with `utilization` and `resets_at`, and `fetchedAtMs` as the snapshot age. `SourceAdapter::quota` became **`quotas`, returning a `Vec`**, since one source can meter several windows at once. `utilization` arrives as a JSON float (`51`, `38.5`, `85.575`); `percent_to_tenths` is the single boundary where a float enters, converting half-up into the integer tenths everything else carries, clamped to `0..=1000`. A window whose `resets_at` has passed is dropped, because its percentage describes a window that no longer exists — and `quotas_at(now)` takes the clock as an argument so this is testable without pinning fixtures to today's date. The **429 heuristic survives as a fallback only**: when the cache is absent or entirely stale, `You've hit your session limit · resets 7:20pm (America/Panama)` still proves a window is exhausted (parsed via `chrono-tz`, minute-less "7pm" included). First real read: 49% left this session, 62% left this week, matching `/usage` exactly.

Presentation follows the same rule in both places: since a source can meter several windows, only the one that **binds first** (least remaining) is shown per source, and every window stays one hover or one click away. The dashboard header shows `claude code · 49% left this session` with all windows, resets, and the snapshot age in the tooltip; the menu bar title shows `302K · claude 49% · codex 63%` and the tray menu lists each window in full (`claude · 49% of the session left · resets Jul 29, 6:50PM`). Deliberately left out for now: `extra_usage`/`spend` in the same cache (credits at 86% of a $40 monthly cap) — that is money against a purchased balance rather than a plan allowance, so it belongs with cost reporting, not here.

### Build separate adapters

Create independent adapters for:

- Cursor
- Claude Code
- Codex

Each adapter should discover or receive its source location, parse records, and pass normalized records to the shared import pipeline. A malformed record in one source should not stop the other sources from importing.

### Add persistence

Introduce SQLite when imports begin:

1. Store the database in the user's macOS Application Support directory.
2. Add versioned migrations.
3. Add unique source identifiers or hashes to prevent duplicate imports.
4. Preserve import errors without storing full sensitive prompts unnecessarily.
5. Recompute dashboard summaries from stored normalized records.

### Add manual refresh

Add one clear `Refresh logs` action:

1. Scan the three configured sources.
2. Import only new records.
3. Report imported, skipped, and failed counts.
4. Refresh the dashboard.

Do not add background file watchers, launch agents, or scheduled jobs yet.

## 10. Privacy and filesystem rules

- Process logs locally.
- Do not upload prompts, transcripts, paths, or usage records.
- Store only fields required for usage reporting.
- Avoid copying raw prompt and response text into the database.
- Show users which folders will be read before future distribution.
- If macOS permissions block automatic access, use a folder picker rather than requesting broader access than needed.

## 11. Preparing for installation on other Macs

After the personal version is stable:

1. Choose the minimum supported macOS version.
2. Decide whether to ship Apple Silicon only or a universal build.
3. Create a final app icon and metadata.
4. Join the Apple Developer Program if public distribution is required.
5. Configure Developer ID signing.
6. Notarize the build with Apple.
7. Package it as a DMG or another supported installer.
8. Test on a clean secondary Mac account or machine.
9. Add an updater only after the installation flow is reliable.

Mac App Store distribution is a separate decision because sandboxing can significantly affect access to local logs.

## 12. Deferred work

- Automatic log watching
- Cloud sync and accounts
- Provider APIs
- Cost-pricing updates
- Menu-bar mode
- Auto-launch
- Cross-device data
- iOS support

## 13. Milestones

1. Rust is installed and the toolchain passes verification.
2. The generated Tauri application runs in development.
3. An empty standalone `.app` opens from Finder.
4. React displays fake usage returned by Rust.
5. The final fake-data MVP passes its acceptance checklist.
6. Cursor, Claude Code, and Codex formats are documented.
7. Manual log import and SQLite persistence are added.
8. Signing and distribution for other Macs are prepared.
