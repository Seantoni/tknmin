# Tokens — Full Code Review

**Date:** 2026-07-30 · **Commit:** `66dad67` · **Scope:** whole app (React frontend + Rust/Tauri backend)
**Focus, as requested:** user UX, load times, error handling, bug detection, and readiness to scale.

---

## Verdict

The architecture is genuinely good. The single-owner refresh rule, the reader/writer capability split, the
revisioned snapshot contract, and the "last-known-good stays on screen" discipline are all decisions that most
apps get wrong and this one gets right. 224 Rust tests back it up. That foundation is worth protecting.

The problems are almost entirely in **one layer**: the read path. Every query in the app is a full-table scan
plus a full JSON deserialize, executed on the main thread, and the store already holds **48,796 records / 52 MB
of payload after roughly two days of use**. That single fact produces most of the load-time findings, the WAL
bloat, the menu-bar cost, and the ceiling on scaling. Fix the read path and a large fraction of this document
disappears.

Second cluster: the frontend is **honest about data but quiet about failure**. It carefully preserves stale
numbers when a refresh fails — and then never tells the user it failed. There is no error boundary, several
promises are fired with `void` and no `catch`, and the one component whose job is to say how old the data is
freezes its own clock.

| Area | State | Note |
| --- | --- | --- |
| Architecture & layering | Strong | Genuinely well-reasoned; don't disturb it |
| Rust test coverage | Strong | 224 tests, meaningful integration tests |
| Read-path performance | **Critical** | Full scans, main thread, unindexed |
| Error surfacing to user | **Weak** | Errors captured, then not shown |
| Frontend robustness | **Weak** | No error boundary, unhandled rejections |
| First-run / empty UX | Weak | No guidance at all |
| Accessibility | Weak | Focus invisible; key info tooltip-only |
| Frontend tests / lint / CI | Absent | 0 TS tests, no ESLint, no CI |

---

## Measurements taken

Grounding numbers, measured against the live store at `~/Library/Application Support/com.josep.tokens/`:

| Metric | Value |
| --- | --- |
| Records stored | 48,796 (claude_code 5,831 · codex 41,729 · cursor 1,234) |
| Payload JSON | 52 MB |
| `usage.sqlite3` | 73 MB |
| `usage.sqlite3-wal` | **66 MB** (before checkpoint) |
| Full scan: SQL fetch | 0.027 s |
| Full scan: JSON parse | 0.221 s |
| **Full scan total** | **~0.25 s** |
| `EXPLAIN QUERY PLAN` on the read query | `SCAN records` — no index used |

One `dashboard_snapshot` call performs **three** of those scans (two summaries + recent) plus a count, in both
windows, on every committed revision. Roughly **0.75 s of main-thread work per window per revision, today**.

> **Note:** while diagnosing the WAL, I ran `PRAGMA wal_checkpoint(PASSIVE)` against the live database. It is a
> non-destructive maintenance operation (it moves WAL pages into the main file), but you should know it
> happened. It reduced the WAL from 66 MB to ~766 pages, which is what confirms checkpoint *starvation* rather
> than write volume as the cause.

---

# P0 — Critical (performance & scaling wall)

### F1. Every read is a full-table scan with a full JSON deserialize; the indexes are dead

**Where:** [src-tauri/src/repository/sqlite.rs:100](src-tauri/src/repository/sqlite.rs#L100),
[:117](src-tauri/src/repository/sqlite.rs#L117), [:211](src-tauri/src/repository/sqlite.rs#L211),
[:220](src-tauri/src/repository/sqlite.rs#L220) · [src-tauri/src/repository/mod.rs:213](src-tauri/src/repository/mod.rs#L213)

`load_matching` issues `SELECT payload FROM records` with **no `WHERE` clause**, deserializes all 48k records
into `UsageRecord` structs, and then filters them in Rust via `summarize::matches`. `summary()` doesn't even do
that — it calls `load_all` and filters afterwards. `assemble_snapshot` then calls `summary()` twice and
`recent()` once, so a single snapshot fetch scans and parses the entire table three times.

The indexes `records_by_source_time` and `records_by_adapter_time`
([sqlite.rs:140-143](src-tauri/src/repository/sqlite.rs#L140-L143)) are never used by any query — confirmed by
`EXPLAIN QUERY PLAN`. They cost write time and disk and return nothing.

**Why it matters:** this is the app's scaling ceiling. 48k records in ~2 days extrapolates to ~750k/month. At
that size a single scan is ~4 s and a snapshot is ~12 s. The dashboard only ever displays 30 days and 50 rows;
it is reading everything, forever, to produce that.

**Recommended fix (in order):**
1. Push the filter into SQL: `WHERE source_app IN (...) AND event_timestamp_utc >= ? AND < ?`. The index
   already exists and would immediately be used.
2. Denormalize the aggregate fields into columns — `input`, `output`, `cached_input`, `reasoning`,
   `display_total`, `cost_minor`, `cost_currency`, `model`. Then `summary()` becomes a `SELECT SUM(...) ...
   GROUP BY model` that never touches `payload` at all.
3. Keep `payload` only for `recent()`, which needs at most `LIMIT 50` rows.
4. Push `ORDER BY event_timestamp_utc DESC LIMIT ?` into SQL instead of `take_recent`
   ([summarize.rs:78](src-tauri/src/repository/summarize.rs#L78)), which currently sorts the whole table in
   memory.

This is the single highest-leverage change in the codebase.

---

### F2. Every Tauri command runs on the main thread

**Where:** [src-tauri/src/commands.rs](src-tauri/src/commands.rs) — no command is `async`, none is
`#[tauri::command(async)]`

Tauri v2 executes non-`async` commands on the main thread. Combined with F1, that means the ~0.75 s snapshot
read **blocks the UI event loop**. Worse:

- [`connect_cursor_dashboard`](src-tauri/src/commands.rs#L167) performs a **blocking HTTPS request** with a
  20 s timeout ([cursor.rs:124](src-tauri/src/adapters/cursor.rs#L124) →
  [:304](src-tauri/src/adapters/cursor.rs#L304)). The Settings button shows "syncing…" but the entire webview
  is frozen while it runs.
- [`test_notification`](src-tauri/src/commands.rs#L219) calls `request_auth_blocking()`.
- [`set_options`](src-tauri/src/commands.rs#L117) writes to disk and re-evaluates alerts.

**Fix:** annotate every command `#[tauri::command(async)]` (cheapest, no signature changes), or convert to
`async fn` + `tauri::async_runtime::spawn_blocking` for the I/O-bound ones. Verify with a deliberate slow
command that the window still repaints.

---

### F3. The menu bar recomputes an all-time, unbounded summary on every change

**Where:** [src-tauri/src/menubar.rs:41](src-tauri/src/menubar.rs#L41),
[:57-67](src-tauri/src/menubar.rs#L57-L67) · called from [lib.rs:172](src-tauri/src/lib.rs#L172)

`current_summary` runs `summary(&SummaryQuery::default())` — no date bound at all, so it is the *most*
expensive possible query, and it runs a **fourth** full scan on every `data_changed` revision. It also runs
during setup ([menubar.rs:18](src-tauri/src/menubar.rs#L18)), before the app can answer any command, so app
launch is now proportional to total history.

**Fix:** after F1 this becomes a cheap aggregate query. Additionally, throttle tray repaints (once per second
is far more than the menu bar needs) and rebuild the menu only when its contents actually change — `set_menu`
currently reconstructs every item on every revision.

---

### F4. The hidden mini window silently doubles all backend load

**Where:** [src-tauri/src/mini.rs:33-55](src-tauri/src/mini.rs#L33-L55) ·
[src/main.tsx:11](src/main.tsx#L11) · [src/hooks/useQuotas.ts:44](src/hooks/useQuotas.ts#L44)

The compact window is built at startup with `.visible(false)`, but it still loads `index.html`, boots a second
WebKit process, mounts `MiniView`, and subscribes to `data-changed` — so it fetches a **full dashboard
snapshot** on every revision, forever, while invisible.

It asks for `{ limit: 0 }`, but that only skips the recent list; the backend still runs both full summaries and
the count ([mod.rs:218-226](src-tauri/src/repository/mod.rs#L218-L226)). So the invisible window costs almost
exactly as much as the visible one.

**Fix (three independent wins):**
1. Build the mini window lazily on first `show_mini`, not during setup — this also removes a second webview
   from cold start.
2. Add a dedicated `quotas_and_health` command; the mini view needs nothing else.
3. Skip refetching while the window is hidden (listen for visibility, refetch once on show).

---

### F5. The WAL grows without bound because long reads starve checkpointing

**Observed:** `usage.sqlite3` 73 MB, `usage.sqlite3-wal` **66 MB**, live. A manual passive checkpoint dropped
it to ~3 MB.

**Cause:** SQLite auto-checkpoints at 1000 WAL pages, but a checkpoint cannot complete while a read transaction
is open. F1 keeps a read transaction open for ~0.25 s at a time, continuously, from two windows plus the menu
bar. The checkpointer never gets a clean window.

**Why it matters:** the WAL is nearly the size of the database, disk usage roughly doubles, and reads get
slower as the WAL grows — a compounding loop with F1.

**Fix:** F1 mostly cures this on its own. Belt and braces: set `PRAGMA wal_autocheckpoint`, and have the
coordinator run a `wal_checkpoint(TRUNCATE)` on the idle path (e.g. alongside the 12-minute full reconcile).

---

### F6. No retention policy; the store grows forever

The dashboard shows a 30-day rolling window
([useUsageDashboard.ts:40](src/hooks/useUsageDashboard.ts#L40)), but nothing is ever pruned. At the observed
ingest rate that's ~750k rows/month, indefinitely.

**Fix:** decide the retention contract explicitly. Options: (a) prune raw records past N days but keep daily
rollups, (b) keep everything but make the dashboard strictly index-driven (F1 makes this viable), (c) a
user-visible "keep N months" setting. (a) is the one that scales best and preserves history for future charts.

---

# P1 — High (bugs & error handling)

### F7. The entire home directory is watched recursively; any `.jsonl` anywhere triggers a sync

**Where:** [src-tauri/src/adapters/claude_code.rs:141-147](src-tauri/src/adapters/claude_code.rs#L141-L147) ·
[refresh.rs:958](src-tauri/src/refresh.rs#L958) · [refresh.rs:970-987](src-tauri/src/refresh.rs#L970-L987)

The Claude Code adapter adds `config_file.parent()` — i.e. **`$HOME`** — as a watch root so it can see
`.claude.json` being atomically replaced. The coordinator then watches every root with
`RecursiveMode::Recursive`.

Two consequences:

1. **FSEvents delivers callbacks for every file change anywhere in the user's home directory** — Downloads,
   `node_modules`, build output, everything. Constant wake-ups, on battery.
2. `classify` resolves any unmatched path to the `$HOME` owner, so **`~/Downloads/data.jsonl` or any `.jsonl`
   in any project directory returns `LocalSourceChanged(ClaudeCode)`** and schedules a real refresh job. The
   existing test at [claude_code.rs:1291](src-tauri/src/adapters/claude_code.rs#L1291) asserts `$HOME` is a
   root — it's intentional, but the recursion isn't.

**Fix:** let `watch_roots` return roots with an explicit recursion mode, watch `$HOME` **non-recursively**, and
make `classify` require the exact filename `.claude.json` when the matched owner is the `$HOME` root. Add a
test for `~/Downloads/data.jsonl → None`.

---

### F8. A refresh failure after the first successful load is completely invisible

**Where:** [src/hooks/useUsageDashboard.ts:103-109](src/hooks/useUsageDashboard.ts#L103-L109) ·
[src/App.tsx:141](src/App.tsx#L141)

The hook does the right thing — on failure it keeps the last good data and sets `error`, deliberately leaving
`status === "ready"`. But `App.tsx` renders the error block **only when `status === "error"`**. So after the
first successful load, `error` is set and then never shown anywhere.

The user sees stale numbers, with the freshness row also frozen (see F11), and nothing indicating a problem.
Same shape in [useQuotas.ts:56](src/hooks/useQuotas.ts#L56) / [MiniView.tsx:72](src/components/MiniView.tsx#L72),
where the error only surfaces if there were no quotas at all.

**Fix:** render a persistent, non-blocking strip whenever `error !== null`, regardless of `status` — *"couldn't
refresh · showing data from 14:32 · retry"*. This is the counterpart the "keep last-known-good" design is
missing.

---

### F9. The revision guard advances before the fetch and is never rolled back on failure

**Where:** [src/hooks/useUsageDashboard.ts:136-138](src/hooks/useUsageDashboard.ts#L136-L138) ·
[src/hooks/useQuotas.ts:70-72](src/hooks/useQuotas.ts#L70-L72)

```ts
if (event.payload.revision <= revision.current) return;
revision.current = event.payload.revision;   // recorded before the fetch
void load(selectedRef.current);              // may throw
```

If that load fails, the UI still holds older data but has already recorded the newer revision. Any subsequent
event at that same revision is discarded. Only a strictly *higher* revision recovers — and during a quiet
period there may not be one for hours.

**Fix:** advance `revision.current` only on success (or restore the previous value in the `catch`). Track the
"highest seen" and "highest rendered" separately.

---

### F10. "Sync now" gives no feedback and frequently appears to do nothing

**Where:** [src/App.tsx:80-88](src/App.tsx#L80-L88) ·
[src/hooks/useUsageDashboard.ts:190-194](src/hooks/useUsageDashboard.ts#L190-L194)

`requestSync` fires and forgets with `.catch(() => {})`. It does not call `load`, so `isRefreshing` never
becomes true and the button never enters its `…` state. And if the sync finds nothing new, the commit doesn't
advance ([refresh.rs:840](src-tauri/src/refresh.rs#L840)) so no event is emitted — **the click produces no
observable effect whatsoever**.

Meanwhile `is_syncing` ([commands.rs:105](src-tauri/src/commands.rs#L105)) and `fetchIsSyncing`
([api/usage.ts:111](src/api/usage.ts#L111)) exist, do exactly the right thing, and are wired to nothing.

**Fix:** set a local `isSyncing` immediately on click, poll/subscribe to lane status, and end with an explicit
terminal state — *"up to date · nothing new"* — so the null result is a result. This is the single most
noticeable UX fix in the list, because it is the only button whose job is reassurance.

---

### F11. Relative ages freeze — in the component whose entire purpose is freshness

**Where:** [src/components/SourceHealth.tsx:48-51](src/components/SourceHealth.tsx#L48-L51) ·
[src/format.ts:336](src/format.ts#L336)

`formatAge` is evaluated during render with `now = new Date()`, and the component only re-renders when a new
snapshot arrives. But `materially_differs`
([repository/mod.rs:272](src-tauri/src/repository/mod.rs#L272)) deliberately excludes `last_attempt_at`, so a
quiet period publishes nothing at all.

Result: the row says "app synced 5s ago" and keeps saying it, indefinitely, while the data ages. Exactly the
false confidence the module's own doc comment says it exists to prevent.

**Fix:** a single shared 15–30 s ticker (`useNow()`) driving the health row, the mini-view reset lines, and the
quota tooltips. Cheap, and it makes the whole freshness story finally true.

---

### F12. No error boundary, and an unknown source value crashes the whole dashboard

**Where:** [src/main.tsx:19](src/main.tsx#L19) ·
[src/components/SourceHealth.tsx:30-32](src/components/SourceHealth.tsx#L30-L32)

```ts
SOURCE_LABEL[left.sourceApp].localeCompare(SOURCE_LABEL[right.sourceApp])
```

If Rust ever sends a `sourceApp` the TS map in [theme.ts](src/theme.ts) doesn't know, this is
`undefined.localeCompare(...)` → TypeError → React unmounts the entire tree → **blank window, no message, no
recovery except relaunch**. There is no `ErrorBoundary` anywhere.

This is a direct scaling hazard: adding a fourth adapter in Rust (Gemini CLI, Copilot, Aider…) and forgetting
to update `theme.ts` turns a missing label into a white screen.

**Fix:** (a) wrap the roots in an `ErrorBoundary` with a visible message and a reload button; (b) replace the
raw map lookups with `sourceLabel(app)` / `sourceColor(app)` helpers that fall back to the raw string and a
neutral grey. Then a new source degrades to "unstyled but working".

---

### F13. The clipboard handoff can fail silently

**Where:** [src/hooks/useThresholdAlerts.ts:52](src/hooks/useThresholdAlerts.ts#L52) ·
[src/App.tsx:112-114](src/App.tsx#L112-L114)

`navigator.clipboard.writeText` requires a secure context and a focused document. In a Tauri custom-protocol
webview this is not dependable, and the call site is `void createHandoff(key)` with no `catch` — so a rejection
is an unhandled promise and the user presses "create handoff" and *nothing happens*, with no error.

The app already depends on `tauri-plugin-clipboard-manager`
([Cargo.toml:36](src-tauri/Cargo.toml#L36)) and uses it correctly from Rust
([alerts.rs:216](src-tauri/src/alerts.rs#L216)).

**Fix:** route the frontend copy through the plugin (or a small Rust command), and show a failure state.

---

### F14. Settings writes to disk and re-evaluates alerts on every keystroke

**Where:** [src/components/SettingsView.tsx:187-191](src/components/SettingsView.tsx#L187-L191) →
[App.tsx:52](src/App.tsx#L52) → [useOptions.ts:34](src/hooks/useOptions.ts#L34) →
[prefs.rs:39](src-tauri/src/prefs.rs#L39) → [commands.rs:137](src-tauri/src/commands.rs#L137)

Typing `100` into a threshold field triggers **three** full round-trips: three atomic temp-file writes, three
alert re-evaluations, three `threshold-alerts` emits — and, per F2, three main-thread blocks.

Separately, the input is controlled and clamped on *every* change, so the field can't be cleared and editing
fights the user (delete-to-empty becomes `0`, and a leading digit can't be typed cleanly).

**Fix:** hold a local draft (as a string), debounce the save ~400 ms or save on blur, and clamp on commit
rather than on keystroke. Show an explicit "saved" tick instead of the transient "saving…".

---

### F15. Unhandled rejections on options save and alert actions; the UI can show an unsaved setting as saved

**Where:** [src/hooks/useOptions.ts:38-49](src/hooks/useOptions.ts#L38-L49) ·
[src/App.tsx:52-54](src/App.tsx#L52-L54) · [src/App.tsx:111](src/App.tsx#L111)

`update()` sets the new options optimistically, then `throw`s on failure — but the caller is
`void updateOptions(next)`, so the rejection is unhandled *and* the optimistic value stays in state. The
checkbox reads "on" while the file on disk says "off". `void continueAlert(key)` has the same missing `catch`.

**Fix:** catch at the call site, revert to the last-persisted options on failure, and render the message in the
`settings-error` slot that already exists ([SettingsView.tsx:298](src/components/SettingsView.tsx#L298)).

---

### F16. Closing the dashboard quits the app and takes the menu-bar item with it

**Where:** [src-tauri/src/lib.rs:191-198](src-tauri/src/lib.rs#L191-L198)

Close is only prevented when the mini window happens to be visible. Otherwise ⌘W / the red dot terminates the
process — which for a menu-bar-resident app is surprising: the tray icon vanishes along with the window, and
the whole "always there, always current" premise with it.

**Fix:** always `prevent_close()` and hide. Quit exclusively via the tray's "Quit Tokens"
([menubar.rs:210](src-tauri/src/menubar.rs#L210)) and ⌘Q.

---

### F17. Falling back to an in-memory store is invisible to the user

**Where:** [src-tauri/src/lib.rs:136-149](src-tauri/src/lib.rs#L136-L149)

If SQLite can't open, the app silently runs in memory and only `eprintln!`s. The user gets an app that looks
fine, re-imports everything on every launch, loses the instant-relaunch property, and is never told why.

**Fix:** record the degraded mode in `AppState` and surface it — the health row is the natural home, or a
one-line banner: *"running without a local store · data will be re-read each launch"*.

---

### F18. One panic permanently bricks the store until relaunch

**Where:** [src-tauri/src/repository/sqlite.rs:88-90](src-tauri/src/repository/sqlite.rs#L88-L90),
[:283](src-tauri/src/repository/sqlite.rs#L283)

A poisoned `Mutex` maps to `RepositoryError::Unavailable` — permanently. Any panic in any refresh worker while
holding the connection lock makes every subsequent read *and* write fail with "the usage store is unavailable"
for the life of the process, with no recovery path.

**Fix:** `parking_lot::Mutex` (no poisoning) or recover explicitly via `PoisonError::into_inner`. A panic
mid-transaction is already safe — SQLite rolls back.

---

### F19. Blocking notification-permission request during setup delays launch

**Where:** [src-tauri/src/lib.rs:98](src-tauri/src/lib.rs#L98) →
[alerts.rs:147-173](src-tauri/src/alerts.rs#L147-L173)

`request_auth_blocking()` runs on the main thread inside `setup`, before the app can serve any command — and it
may present a system dialog. In development it additionally fails `check_bundle()` on every single launch.

**Fix:** move it off the setup path (spawn it), and ask on the first threshold crossing or explicitly from
Settings, where the user has context for the request.

---

# P2 — Medium (UX)

### F20. There is no first-run experience

[App.tsx:152](src/App.tsx#L152) shows *"No usage recorded yet."* and nothing else. It doesn't say which
directories are being watched, whether each source was found, that Cursor needs connecting for real token
counts, or that data will appear on its own. The `sources` and `health` arrays needed to say all of this are
already loaded and sitting unused in that branch.

**Fix:** turn the empty state into a checklist — one row per source, found/not found, with a "connect" action
for Cursor and the watched path (or a friendly description of it) for the others.

---

### F21. Critical information lives only in native `title` tooltips

**Where:** [App.tsx:66](src/App.tsx#L66) · [MiniView.tsx:150](src/components/MiniView.tsx#L150) ·
[SourceStrip.tsx:91](src/components/SourceStrip.tsx#L91),
[:127](src/components/SourceStrip.tsx#L127) · [SourceHealth.tsx:43](src/components/SourceHealth.tsx#L43) ·
[StatBar.tsx:56](src/components/StatBar.tsx#L56) · [EventList.tsx:21](src/components/EventList.tsx#L21)

Quota breakdowns, freshness detail, total gaps, and per-field quality caveats are all carried exclusively by
the `title` attribute. Native tooltips have a ~1 s delay, can't be styled, can't be copied, are unreliable on
keyboard focus, and are invisible on touch/trackpad-only exploration. A user who never hovers never learns that
a total is incomplete.

**Fix:** promote the two or three that carry real caveats (incomplete totals, source freshness) into visible
secondary text, and use a small styled popover for the rest. Keep `title` only as redundancy.

---

### F22. Keyboard focus is invisible

**Where:** [src/App.css:275](src/App.css#L275), [:365](src/App.css#L365) — `outline: none`, with no
`:focus-visible` replacement for `.ghost`, `.event-row`, `.strip-row`, `.disclosure`, `.menu-trigger`, `.more`

The markup is good — everything interactive really is a `<button>` — but a keyboard user cannot see where they
are. `:focus` styles exist only for two form controls in Settings.

**Fix:** one global `:focus-visible { outline: 2px solid var(--muted); outline-offset: 2px; }` rule. This is a
five-line fix with disproportionate benefit.

---

### F23. The event list silently caps at 50 with no indication

**Where:** [src/api/usage.ts:43](src/api/usage.ts#L43) · [src/App.tsx:183](src/App.tsx#L183) ·
[src/components/EventList.tsx:174-178](src/components/EventList.tsx#L174-L178)

The header shows the true filtered record count — which for this store would read *5,831* — while the list can
never contain more than 50 rows. "Show 42 more" expands to 50 and then the control simply disappears. Nothing
says *"showing 50 of 5,831"*, and there is no way to see row 51.

**Fix:** label the list honestly (`showing 50 of 5,831`) and add either "load 50 more" or a date-range control.
Given F1's rewrite, `LIMIT`/`OFFSET` pagination becomes trivial at the same time.

---

### F24. A source with a live allowance but no recent usage is invisible

**Where:** [src/components/SourceStrip.tsx:38](src/components/SourceStrip.tsx#L38) ·
[summarize.rs:149](src-tauri/src/repository/summarize.rs#L149)

The strip iterates `overview.bySource`, which is built purely from records. A source that reports a quota but
has no usage in the 30-day window doesn't appear at all — even though its allowance is the thing the user most
wants to see.

**Fix:** render the union of `SourceApp::ALL`, the summary groups, and the quota list.

---

### F25. An expired Cursor session is a footnote, not an action

[cursor.rs:366](src-tauri/src/adapters/cursor.rs#L366) produces an excellent message — *"Cursor rejected the
session token; copy a fresh WorkosCursorSessionToken"* — and then delivers it into the small grey health row at
the bottom of the page ([SourceHealth.tsx:54](src/components/SourceHealth.tsx#L54)).

Session cookies expire routinely. This is a recurring, fully-diagnosed, fully-recoverable failure that the app
knows exactly how to fix, presented as a footnote.

**Fix:** promote auth failures to a banner with a **Reconnect** button that opens Settings with the Cursor
section focused.

---

### F26. Dead API surface — six commands registered and unreachable

`fetchUsageSummary`, `fetchRecentUsage`, `fetchUsageRecordCount`, `fetchUsageQuota`, `reportWindowFocused`,
`fetchIsSyncing` ([api/usage.ts:67-113](src/api/usage.ts#L67-L113)) are all unused, along with their Rust
counterparts registered at [lib.rs:105-126](src-tauri/src/lib.rs#L105-L126).

Each is another full-scan entry point that nothing exercises. `is_syncing` should be *wired up* (see F10); the
rest should probably go.

---

### F27. Mini-window height fitting is unthrottled

[MiniView.tsx:42-55](src/components/MiniView.tsx#L42-L55) issues a `setSize` IPC call on every `ResizeObserver`
callback, and resizing the window can itself re-fire the observer. Throttle to an animation frame and skip when
the height is unchanged.

---

# P3 — Hygiene & scaling foundations

### F28. No frontend tests, no linter, no CI

224 Rust tests vs **0** TypeScript tests. No ESLint, no Prettier, no CI workflow. `pnpm build` is
`tsc && vite build` ([package.json:8](package.json#L8)).

`npx tsc --noEmit` currently passes cleanly with `strict`, `noUnusedLocals`, and `noUnusedParameters` on —
that's a real asset. Add a gate before it drifts.

**Fix:** Vitest + Testing Library for the hooks first (`useUsageDashboard`'s revision handshake is subtle
enough to deserve tests, and F9 is exactly the kind of bug a test would have caught); ESLint with
`react-hooks`; a GitHub Actions job running `tsc --noEmit`, `eslint`, `vitest`, and `cargo test`.

### F29. `csp: null`

[tauri.conf.json:27](src-tauri/tauri.conf.json#L27). Acceptable for a purely local app today; set a real policy
before any remote content is rendered or the app is distributed.

### F30. There is no actual migration mechanism, and timestamps are undocumented milliseconds

[sqlite.rs:198-205](src-tauri/src/repository/sqlite.rs#L198-L205) only bumps the stored version number —
there's no ordered migration runner despite the comment claiming one. F1 requires the first real schema change,
so build the runner as part of that work.

Also: `event_timestamp_utc` stores **milliseconds**, not seconds, which isn't stated anywhere and makes
`datetime(x, 'unixepoch')` silently return nothing. Worth a comment on the column and a named constant.

### F31. A source directory that doesn't exist at launch is never watched

[refresh.rs:957](src-tauri/src/refresh.rs#L957) skips non-existent roots, and
[refresh.rs:370](src-tauri/src/refresh.rs#L370) `std::mem::forget`s the watcher so registrations can never be
revised. Install Claude Code *after* Tokens is running and only the 45 s / 12 min reconcilers will ever notice.
Documented as intentional; consider watching the parent and promoting when the directory appears.

### F32. `macOSPrivateApi: true` and an unsigned build

[tauri.conf.json:13](src-tauri/tauri.conf.json#L13) is required for the transparent mini window, but it rules
out Mac App Store distribution. The build is also unsigned and unnotarized
([README.md:89](README.md#L89)) — fine for this Mac, blocking for anyone else's. Decide the distribution story
before investing in packaging.

---

# Recommended sequencing

Ordered so each phase makes the next one easier, and so user-visible wins land early.

### Phase 0 — One day. Stop the bleeding, unblock everything else.
| # | Change | Why first |
| --- | --- | --- |
| F2 | `#[tauri::command(async)]` on every command | One-line-per-command; immediately unfreezes the UI |
| F12 | `ErrorBoundary` + `sourceLabel()`/`sourceColor()` helpers | A blank window is the worst failure mode; also de-risks adding sources |
| F8 | Show `error` whenever it's set, not only on `status === "error"` | The information is already there and simply isn't rendered |
| F9 | Advance `revision.current` only on success | Two-line fix for a silent permanent-staleness bug |
| F7 | Watch `$HOME` non-recursively; exact-match `.claude.json` | Stops constant home-directory FSEvents churn |

### Phase 1 — The load-time work. This is the main event.
1. **F1** — SQL-side filtering + aggregate columns + `LIMIT` in SQL. Build the migration runner (F30) here.
2. **F3** — menu bar onto aggregates; throttle tray repaints.
3. **F4** — lazy mini window + a dedicated `quotas_and_health` command + no fetching while hidden.
4. **F5** — `wal_autocheckpoint` + periodic `TRUNCATE` checkpoint.
5. **F6** — decide and implement the retention contract.

*Success criterion: a snapshot fetch under 20 ms at 500k records, and a WAL that stays under a few MB.*

### Phase 2 — Error handling & feedback.
F10 (sync-now feedback — highest perceived value), F13 (clipboard via plugin), F14 + F15 (options debounce,
revert, no unhandled rejections), F17 (surface in-memory fallback), F18 (`parking_lot`), F19 (auth off the
setup path), F16 (close means hide).

### Phase 3 — UX polish.
F20 (first-run checklist), F22 (`:focus-visible` — five lines), F11 (live ages), F25 (reconnect banner),
F21 (tooltips → visible text), F23 (honest pagination), F24 (show quota-only sources), F27 (throttle fit).

### Phase 4 — Scaling foundations, before adding features.
F28 (Vitest + ESLint + CI), F26 (delete dead commands, wire `is_syncing`), F29 (CSP), F31 (watcher promotion),
F32 (decide distribution).

---

# Before you scale this app further

Three things should exist before a fourth source, charts, or a second screen are added:

1. **An indexed read path (F1).** Every new feature — charts, date ranges, per-project breakdowns — is another
   query. Adding them on top of "scan and deserialize everything" multiplies the problem instead of adding to
   it. This is the prerequisite for essentially everything else you'd want to build.

2. **A source registry that cannot desync.** Right now adding a `SourceApp` in Rust requires remembering
   [theme.ts](src/theme.ts), [SettingsView.tsx:27](src/components/SettingsView.tsx#L27) (`SOURCE_ORDER`),
   [domain/options.ts:38](src/domain/options.ts#L38), and the `SourceApp` union in
   [domain/usage.ts:8](src/domain/usage.ts#L8) — four places, none of which fail loudly, and one of which
   (F12) white-screens the app. Have Rust emit the source list (label, colour, order) through
   `source_capabilities`, which already exists and already isn't fully used.

3. **A frontend test harness (F28).** The Rust side is well-tested and it shows. The React side carries the
   subtlest logic in the app — the revision handshake, request-id guarding, mount tracking — and has no tests
   at all. F9 is precisely the class of bug that one test would have caught.

---

# What is already good — please don't lose it

Worth stating explicitly, because a refactor could easily erode these:

- **The single-owner refresh rule** ([refresh.rs](src-tauri/src/refresh.rs)) is genuinely well-designed, and
  enforced by the type system via the `UsageReader`/`UsageWriter` split rather than by convention.
- **The revisioned snapshot contract** — subscribe-then-fetch, ignore events at or below the fetched revision —
  correctly closes the race most apps leave open. (F9 is a small hole in the *implementation*, not the design.)
- **Atomic per-source transactions** with checkpoints committed alongside the data they account for. This is
  the one thing incremental ingestion cannot survive getting wrong, and it's right.
- **Integer-only money and percentages**, end to end, including in the formatters. Rare, and correct.
- **`null` means unreported, never zero**, with the caveats carried through to the UI. The intellectual honesty
  of `TokenField.quality` and `undatedRecordsExcluded` is unusual and valuable — most of the P2 items above are
  about making that honesty *visible*, not about adding it.
- **Two separate freshness clocks** (`appSyncedAt` vs `sourceObservedAt`). Exactly right for Cursor's delayed
  billing, and most apps would have collapsed them into one lie.
- **The comments explain *why*, not *what*.** Reviewing this was materially faster because of it.
