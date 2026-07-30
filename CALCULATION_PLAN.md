# Tokens — The Calculation Domain

**Date:** 2026-07-30 · **Scope:** a new pure domain that turns allowance readings into "can I keep going?"
**Status:** planned, not implemented. Decisions below are settled; open questions are marked as such.

---

## What this adds

One question the app cannot currently answer: *if I keep working like this, will I run out before
this resets?*

Today the interface reports position — "7% left this week" — and warns when position crosses a
fixed line ([`domain/notifications.rs:80`](src-tauri/src/domain/notifications.rs#L80)). Position is
not risk. 7% left with two hours to go is comfortable; 40% left with five days to go, spent at the
rate of the last hour, is not. The threshold alert fires at the same percentage either way, and by
then there is nothing to do about it except stop.

This plan adds a **pace** domain: a pure calculation that compares the rate an allowance is being
spent against the rate it can afford to be spent, and states the outcome as time.

> **1h 40m of usage left · resets in 3h** — at this pace you run out 1h 20m early

The value is not the number, it is the *lead time*. A projection can say "you will run out
mid-afternoon" hours before a percentage threshold trips, which is the difference between choosing
to hand off and being cut off mid-task. Given that the handoff flow is the app's signature workflow
([`OVERVIEW.md`](OVERVIEW.md) § The handoff flow), moving its trigger from "nearly out" to "on
track to be out" is the highest-leverage thing that can be added to it.

---

## Decisions settled

| Question | Decision |
| --- | --- |
| What sets the state | Runway: will the allowance outlast the window |
| How pace is sourced | Phased — single-reading first, persisted samples second |
| Pace measurement | Worst across a ladder of horizons, floored by window length |
| Primary display | Allowance expressed as time, not percent or ratio |
| Sequencing | Quota-only risk ships **before** the F1 read-path rewrite |
| Where the math lives | Rust, `src-tauri/src/domain/pace.rs`, pure |

---

# 1. The model

## 1.1 One integer expression

An allowance reading gives remaining allowance; a pace measurement gives a rate. Divide:

```text
runway_minutes = remaining_tenths × elapsed_minutes / consumed_tenths
```

- `remaining_tenths` — tenths of a percent unspent, already
  [`UsageQuota::remaining_percent_tenths()`](src-tauri/src/domain/quota.rs#L43)
- `consumed_tenths` — tenths spent between two observations of the same window instance
- `elapsed_minutes` — minutes between those two observations

Every term is an integer, so the whole calculation stays inside the codebase's rule that percentages
and money never become floats. No intermediate rounding: one division, at the end, in `u64`.

Compare `runway_minutes` to `T`, the minutes until reset, and everything else falls out:

| Quantity | Expression |
| --- | --- |
| Shortfall (minutes early it runs out) | `T − runway` when positive |
| Slack (minutes of allowance to spare) | `runway − T` when positive |
| Pace as a multiple of what's affordable | `T / runway` |
| Projected exhaustion instant | `now + runway_minutes` |

The headline display *is* `runway_minutes`. The risk metric and the display metric are the same
computation, which is why this framing is worth preferring over the equivalent "compare two
durations" or "compare two rates" formulations.

## 1.2 Worked examples

All verified arithmetic. `R` = remaining tenths, `T` = minutes to reset.

**Exactly on the line.** Claude 5-hour session. `R = 500` (50% left), `T = 60`. The last 30 minutes
consumed 250 tenths.

```text
runway = 500 × 30 / 250 = 60 min       runway == T  →  Amber (too close to call)
```

**Over the line.** Same position, but those 250 tenths went in 15 minutes.

```text
runway = 500 × 15 / 250 = 30 min       T − runway = 30  →  Red, "runs out 30m early"
                                       T / runway = 2.0×
```

**Further along.** 15 minutes later: 250 tenths spent, so `R = 250`, `T = 45`. Trailing measurement
still 250 tenths per 15 minutes.

```text
runway = 250 × 15 / 250 = 15 min       T − runway = 30  →  Red, "runs out 30m early"
                                       T / runway = 3.0×
```

Note the self-normalising property: `R` and `T` are re-read every time, so the calculation
automatically forgives an earlier burst if the user then idles, and automatically tightens as the
window drains. There is no cumulative state to reset.

**Comfortable.** Cursor billing cycle, `window_minutes = 44640` (31 d). `used = 120` (12%),
`R = 880`, `T = 30240` (21 d). Since-open basis: `elapsed = 44640 − 30240 = 14400` (10 d).

```text
runway = 880 × 14400 / 120 = 105,600 min ≈ 73 d      runway ≫ T  →  Green
```

**Idle.** `consumed_tenths == 0` over every horizon → no pace exists → Green,
`projected_exhaustion_at = None`. No division guard needed as a special case; it is the definition.

**Rolling recovery.** `consumed_tenths < 0`, because a rolling window returned allowance as old
usage aged out → treated as no burn → Green, and the basis records that the window is recovering.

## 1.3 What this framing removes

Worth recording, because an earlier draft of this plan carried all three as work items.

**No `WindowKind` on `UsageQuota`, and no adapter changes.** The trailing basis needs only two
readings of the same window instance. It never asks when the window opened, so it never needs to
know whether the window is anchored (Cursor's calendar billing cycle, Claude's 5-hour session) or
rolling. All three adapters stay untouched.

**No division-by-zero branch.** An idle user has no measurable pace, which is the same thing as
unlimited runway. It is the natural reading, not an edge case.

**No hysteresis ledger.** See § 2.2 — the uncertainty band does this job, statelessly.

## 1.4 What it does *not* remove

Precision matters more than the sales pitch here.

**The since-open basis does need `window_minutes`, and does assume an anchored window.** With a
single stored reading there is no second point, so the second point has to be the window's opening
at zero used:

```text
elapsed  = window_minutes − T
consumed = used_percent_tenths
```

That is only literally true for an anchored window. For a rolling window, `resets_at` marks when the
oldest counted usage expires, so `window_minutes − T` is approximately the age of the oldest
still-counted request, and `used / elapsed` is the average pace across the usage actually inside the
window. That is a meaningful number. What is *not* true for a rolling window is that the allowance
only drains — it also returns, so a runway computed this way is a **lower bound**, i.e. it errs
toward Red.

Erring toward Red is the acceptable direction for a risk indicator, so the since-open basis is kept
for every window kind, with that caveat recorded in the type (§ 3.1) rather than papered over.

**Short-horizon pace is invalid for long windows.** A weekly allowance's affordable pace implicitly
assumes seven days of continuous work. Nobody works seven days continuously, so extrapolating a
30-minute burst across a week says "you will exhaust the week by Tuesday" for any ordinary working
session. That is a false positive, and shipping it would destroy trust in the indicator faster than
anything else in this plan.

The correct projection for a long window needs the user's **duty cycle** — how many active hours a
day they typically have — which is exactly the historical baseline in Phase C. Until then:

| Window length | Shortest horizon permitted | Why |
| --- | --- | --- |
| ≤ 1440 min (a day or less) | `max(window/40, 10 min)` | Continuous-use assumption roughly holds inside a session |
| > 1440 min | `max(window/10, 10 min)` only | A burst says nothing about a week without a duty cycle |

This is why Claude's 5-hour session is where the feature works immediately, and Cursor's 31-day
cycle needs Phase C to be sharp. Say so in the interface rather than implying equal confidence.

---

# 2. States

## 2.1 The six states

```rust
pub enum PaceState {
    /// No window is running. Claude reports a rolling session as zero used with
    /// no reset until the first request; the allowance is whole and there is no
    /// pace to measure. Not a degraded reading — a real, current statement.
    NotStarted,
    /// Comfortably inside budget: runway exceeds the window by more than the
    /// measurement's own uncertainty.
    Green,
    /// Within the uncertainty band. The data cannot support a finer claim.
    Amber,
    /// Projected to run out before reset, by more than the uncertainty.
    Red,
    /// Nothing left.
    Exhausted,
    /// No pace could be measured, or the reading is too old to project from.
    Unknown,
}
```

`Unknown` and `NotStarted` are separate on purpose, and neither may ever render as Green. A
dashboard that shows green when it does not know is worse than one that shows nothing.

## 2.2 The band, and why Amber is honest

```text
tolerance_minutes = max(T / 10, reading_age_minutes)

Green  : runway > T + tolerance
Amber  : |runway − T| ≤ tolerance
Red    : runway < T − tolerance
```

Amber is not a comfort margin picked to feel prudent. It is the region where the inputs genuinely
cannot distinguish "makes it" from "doesn't", and it is sized from the actual sources of error:

1. **Quantisation.** `used_percent_tenths` is an integer, so a measurement over a small `consumed`
   carries a relative error of roughly `1 / consumed`. Below a floor (proposed: 20 tenths, i.e. two
   percentage points) the trailing measurement is too coarse to use at all and the basis falls back.
2. **Staleness.** The reading is `age = now − observed_at` old. Consumption during that gap is
   unmeasured, so including `age` in the tolerance means a 40-minute-old reading refuses to make
   40-minute-fine distinctions. This falls out of one term and needs no separate rule.
3. **Sample spacing.** `elapsed` is quantised by the ~60-second quota lane.

Two properties come free from this shape:

- **Green and Red cannot flip into each other.** Any transition passes through Amber, so there is no
  flapping between the two states a user would act on — and therefore **no hysteresis ledger, and no
  state held between evaluations**. The evaluator stays a pure function of its inputs, the way
  [`evaluate_alerts`](src-tauri/src/domain/notifications.rs#L80) is. This matters more than it
  looks: `merge_quota` ([`repository/mod.rs:246`](src-tauri/src/repository/mod.rs#L246)) treats any
  fresher `observed_at` as a change, so the revision advances on essentially every poll, and a naive
  boundary comparison really would flip on a 60-second cadence.
- **A stale reading degrades gracefully** rather than in a step, until it crosses the hard limit
  below.

If flapping between Green and Amber proves annoying in practice, a dwell ledger goes in a new
`src-tauri/src/pace.rs` sitting *above* the domain — mirroring how the fired/snoozed alert ledger
sits outside `notifications.rs` so that module stays free of state and I/O. Not before it is
observed.

## 2.3 When the reading is too old

```text
Unknown when reading_age > max(window_minutes / 10, 15 min)
```

| Window | Limit |
| --- | --- |
| Claude 5-hour session (300) | 30 min |
| Weekly (10080) | 16.8 h |
| Cursor billing cycle (44640) | 3.1 d |

Long windows tolerate old readings because a week's pace does not change in an hour.

**Staleness cuts the wrong way here, which is why this rule is not optional.** `resets_at` is an
absolute instant, so `T` is always live. But `used_percent_tenths` is only as fresh as
`observed_at`. A stale reading therefore *overstates* what remains while the clock keeps running,
which inflates the affordable pace and paints the situation greener than it is. Every other
staleness bug in this app is cosmetic; this one is directionally dangerous.

## 2.4 State selection order

Checked in this order, first match wins:

1. `resets_at` is `None` → `NotStarted`
2. `remaining_tenths == 0` → `Exhausted`
3. quota is not current at `now`
   ([`is_current_at`](src-tauri/src/domain/quota.rs#L52)) → excluded upstream, no row emitted
4. reading age over the § 2.3 limit → `Unknown`
5. no horizon yields `consumed_tenths > 0` → `Green`, `projected_exhaustion_at = None`
6. otherwise, the band in § 2.2

---

# 3. Types

## 3.1 Rust

New module `src-tauri/src/domain/pace.rs`, added to the list in
[`domain/mod.rs:7`](src-tauri/src/domain/mod.rs#L7). Pure: no I/O, no Tauri, no clock of its own.

```rust
/// How a pace was measured, so the interface can qualify what it shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PaceBasis {
    /// Two observations, `minutes` apart. Needs no assumption about the window.
    Trailing { minutes: u32 },
    /// The window's own opening, assumed at zero used. Requires an anchored
    /// window; on a rolling one the runway it produces is a lower bound,
    /// because a rolling allowance also returns.
    SinceWindowOpen { assumed_anchored: bool },
}

/// One allowance window's pace and what it implies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowPace {
    // Identity, matching `quota_key` exactly so rows can be joined to quotas.
    pub source_app: SourceApp,
    pub label: Option<String>,
    pub window_minutes: u32,

    pub state: PaceState,

    /// Minutes of usage left at the measured pace. `None` when no pace could be
    /// measured — which is not zero, and must not render as zero.
    pub runway_minutes: Option<u32>,
    /// When the allowance is projected to run out. An absolute instant, so the
    /// interface counts down to it rather than re-deriving it. `None` when the
    /// allowance is projected to outlast the window, or no pace exists.
    pub projected_exhaustion_at: Option<DateTime<Utc>>,
    /// Minutes earlier than the reset. `None` unless the state is `Red`.
    pub shortfall_minutes: Option<u32>,
    /// Minutes of allowance to spare at reset. `None` unless it outlasts.
    pub slack_minutes: Option<u32>,

    /// Pace as an integer percentage of the affordable pace: 240 means 2.4×.
    /// Derived, carried so Rust and the interface cannot round it differently.
    pub pace_ratio_percent: Option<u32>,

    pub basis: Option<PaceBasis>,
    /// The reading this rests on, so age can be shown beside the projection.
    pub observed_at: DateTime<Utc>,
}
```

The evaluator's signature mirrors `evaluate_alerts` deliberately — same shape, same injected `now`,
same purity:

```rust
pub fn evaluate_pace(
    quotas: &[UsageQuota],
    samples: &[QuotaSample],
    now: DateTime<Utc>,
) -> Vec<WindowPace>
```

One `WindowPace` per live quota window, ordered the way
[`quotaGroups`](src/format.ts#L226) already orders them: tightest first, a source's own windows
shortest-first.

```rust
/// One historical observation of one allowance window.
///
/// `resets_at` is carried per sample because it identifies the window
/// *instance*: a delta taken across a reset spans a drop to zero and produces a
/// large negative pace, so samples are only ever differenced within one
/// instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaSample {
    pub source_app: SourceApp,
    pub label: Option<String>,
    pub window_minutes: u32,
    pub used_percent_tenths: u16,
    pub resets_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
}
```

## 3.2 TypeScript

[`src/domain/usage.ts`](src/domain/usage.ts) already carries the header rule: *change these only
alongside the Rust definitions they mirror.* Add `src/domain/pace.ts` with `PaceState`, `PaceBasis`,
and `WindowPace`, and one field on `DashboardSnapshot`
([`usage.ts:232`](src/domain/usage.ts#L232)):

```ts
export interface DashboardSnapshot {
  // …
  /** One row per live allowance window, read at the same revision. */
  pace: WindowPace[];
}
```

`PaceState` is a fifth place a Rust enum must be mirrored, which is exactly the desync hazard
[`CODE_REVIEW.md`](CODE_REVIEW.md) § F12 warns about. Mitigation: every `PaceState` consumer in
TypeScript is written as an **exhaustive switch returning a value** — the pattern
[`describeSyncState`](src/format.ts#L359) and [`describeQuality`](src/format.ts#L416) already use —
so `tsc` fails the build on a new variant rather than the app white-screening on a missing map key.
No `Record<PaceState, …>` lookups.

---

# 4. Measuring the pace

## 4.1 The horizon ladder

A single horizon cannot serve a 5-hour session and a 31-day billing cycle. Instead, compute the
runway over several horizons from the same sample set and **take the worst** (shortest runway):

```text
horizons(window_minutes) =
    if window_minutes <= 1440:
        [ max(window/10, 10), max(window/40, 10), 30 ]
    else:
        [ max(window/10, 10) ]          # § 1.4: no short horizons without a duty cycle
    ∪  [ SinceWindowOpen ]
```

Three integer divisions over one already-loaded slice of samples. Taking the worst means a burst at
any scale the window can plausibly contain is caught, while a pause cannot declare safety — the
since-open rung is always present and always includes the idle time.

A horizon is **discarded** rather than counted when:

- fewer than two points fall inside it,
- the points do not share a `resets_at` (they straddle a reset),
- `consumed_tenths < 20` (§ 2.2 quantisation floor),
- `consumed_tenths <= 0` (idle, or a rolling window recovering).

If every horizon is discarded, the state is Green with no projection — honestly, because nothing was
observed being spent.

## 4.2 The latest point is the live quota row, not a sample

Samples are written **only when the value changes** (§ 5.2), which is what keeps the table small and
makes every stored row a real delta. On its own that would lose the evidence that nothing was spent
recently: if `used` last moved an hour ago, the two newest samples are an hour apart with a delta,
and a naive trailing measurement would report that hour-old pace forever.

The fix is to always use the **live row from `quotas`** — with its own `observed_at` — as the
right-hand endpoint of every measurement, and the sample table only for left-hand endpoints. Then an
idle interval is measured correctly as zero pace, because the endpoint keeps advancing while
`used_percent_tenths` does not.

Consequence for Phase A, before any samples exist: the only available left-hand endpoint is the
window's opening, so Phase A is exactly the `SinceWindowOpen` rung and nothing else. This is not
throwaway work — it remains the permanent cold-start path, because for the first minutes after
launch there are no samples regardless of phase.

---

# 5. Storage

## 5.1 The problem

```153:161:src-tauri/src/repository/sqlite.rs
            CREATE TABLE IF NOT EXISTS quotas (
                source_app          TEXT NOT NULL,
                label               TEXT NOT NULL,
                window_minutes      INTEGER NOT NULL,
                used_percent_tenths INTEGER NOT NULL,
                resets_at           INTEGER,
                observed_at         INTEGER NOT NULL,
                PRIMARY KEY (source_app, label, window_minutes)
            );
```

The primary key is the window identity, so each window holds exactly one row and
[`merge_quota`](src-tauri/src/repository/mod.rs#L246) overwrites it. The ~60-second quota lane is
already producing an ideal time series and the app discards every point but the last.

## 5.2 New table

```sql
CREATE TABLE IF NOT EXISTS quota_samples (
    source_app          TEXT NOT NULL,
    label               TEXT NOT NULL,
    window_minutes      INTEGER NOT NULL,
    used_percent_tenths INTEGER NOT NULL,
    -- Identifies the window instance; deltas never span two.
    resets_at           INTEGER,
    -- Milliseconds since the Unix epoch, like every other instant in this
    -- schema. See CODE_REVIEW.md § F30 — the unit is not currently documented
    -- anywhere and `datetime(x, 'unixepoch')` silently returns nothing.
    observed_at         INTEGER NOT NULL,
    PRIMARY KEY (source_app, label, window_minutes, observed_at)
);

CREATE INDEX IF NOT EXISTS quota_samples_recent
    ON quota_samples (source_app, label, window_minutes, observed_at DESC);
```

**Insert rule.** Write a row only when `used_percent_tenths` or `resets_at` differs from the newest
stored sample for that key. This deliberately diverges from `merge_quota`, which counts a fresher
`observed_at` as a change even when the numbers are identical: that rule is right for freshness
(the interface should say "synced 5s ago") and wrong for a sample table, which should hold deltas
only. § 4.2 explains why nothing is lost.

**Where it is written.** Inside [`apply()`](src-tauri/src/repository/sqlite.rs#L299), in the same
transaction as the quota merge it derives from. A sample that outlived its quota, or vice versa,
would be the same class of bug as a checkpoint outliving its records — and the transaction shape
already exists to prevent exactly that.

**Retention.** Keep `max(2 × window_minutes, 7 days)` per key: enough to measure any horizon plus
context, bounded. Prune on the idle path, alongside the `wal_checkpoint(TRUNCATE)` that
[`CODE_REVIEW.md`](CODE_REVIEW.md) § F5 puts on the 12-minute reconcile.

**Volume.** Bounded by change frequency, not poll frequency. Worst case is one row per key per
minute during heavy use — roughly 1,440 rows/day/window against the ~25,000 *records* per day this
store already ingests. Negligible.

## 5.3 The migration runner has to be built first

[`CODE_REVIEW.md`](CODE_REVIEW.md) § F30: [`migrate()`](src-tauri/src/repository/sqlite.rs#L122)
only bumps a stored version number. There is no ordered runner, despite the comment at
[`sqlite.rs:35`](src-tauri/src/repository/sqlite.rs#L35) claiming one. `quota_samples` is the first
real schema change, so Phase B builds it:

- an ordered `&[(u32, &str)]` of migration steps,
- each applied inside a transaction and recorded before the next runs,
- `SCHEMA_VERSION` 1 → 2,
- the existing "written by a newer version" refusal
  ([`sqlite.rs:193`](src-tauri/src/repository/sqlite.rs#L193)) kept as-is,
- a test that a version-1 store opens, upgrades, and keeps its records.

F1 needs this runner too, so building it here is not a detour.

---

# 6. Placement in the architecture

## 6.1 Layering

Unchanged dependency direction: `domain` depends on nothing, `repository` depends on `domain`.

| Piece | Location | Note |
| --- | --- | --- |
| The math | `src-tauri/src/domain/pace.rs` | Pure, `now` injected, no I/O |
| Sample reads | `UsageReader::quota_samples(since)` | New method on the read trait |
| Sample writes | `apply()` in `repository/sqlite.rs` | Same transaction as the quota |
| Assembly | `assemble_snapshot` | Calls the pure evaluator |
| Transport | `DashboardSnapshot.pace` | Travels with the quotas it describes |
| Display | `format.ts`, `MiniView`, header, `SourceStrip` | Exhaustive switches only |

## 6.2 It must travel in the snapshot

[`assemble_snapshot`](src-tauri/src/repository/mod.rs#L213) gains the pace computation, and
`DashboardSnapshot` ([`mod.rs:168`](src-tauri/src/repository/mod.rs#L168)) gains the field. Not a
separate command.

The reason is the contract the type's own doc comment states: *fetched as a unit so the interface can
never show a total from one revision beside a quota from another.* A pace fetched separately from
the quota it was computed from is precisely that bug, and a worse instance of it — the two would
disagree about how much is left, and one of them would be the number the user acts on.

`InMemoryUsageRepository` gets the same treatment, since the two backends exist to be checked
against each other.

## 6.3 The clock problem, and how the shape avoids it

A runway is a countdown. [`CODE_REVIEW.md`](CODE_REVIEW.md) § F11 documents what happens to
relative times in this app: `formatAge` is evaluated during render, the component only re-renders on
a new snapshot, and `materially_differs`
([`repository/mod.rs:272`](src-tauri/src/repository/mod.rs#L272)) deliberately publishes nothing
during a quiet period — so the freshness row freezes at "synced 5s ago" indefinitely. A frozen
countdown would be the same bug attached to a number people actually act on.

The shape that avoids it, without duplicating the state logic across the language boundary:

- **Rust ships absolute instants** — `projected_exhaustion_at` and the existing `resets_at` — plus
  the already-decided `state`.
- **TypeScript counts down to instants** and never re-derives the state. Formatting a countdown to a
  fixed instant is correct at every tick by construction.
- **The state itself only changes when a reading changes**, which requires a new quota observation
  and therefore a new revision and a refetch.

The residual imprecision is bounded and conservative: if the state is Red and the user then idles,
it stays Red until the next quota reading, ≤60 s away. Erring toward Red is the right direction.

This still wants F11's shared `useNow()` ticker to drive the repaint — it is a prerequisite for the
display, not for the correctness of the values.

## 6.4 Menu bar

[`menubar.rs`](src-tauri/src/menubar.rs) can carry the state as a single glyph beside the token
total, which is the highest-value surface for it: the whole point is a signal you do not have to
open a window to read.

Blocked on [`CODE_REVIEW.md`](CODE_REVIEW.md) § F3. `current_summary`
([`menubar.rs:41`](src-tauri/src/menubar.rs#L41)) already runs an unbounded, all-time
`summary(&SummaryQuery::default())` on every revision — a fourth full scan. Pace is cheap (it reads
quotas and a bounded sample slice, never `records`), but it must not be added to a tray rebuild that
is already the worst offender in the file. Do the F3 work first, then add the glyph.

---

# 7. Display

## 7.1 The headline is time

Decided: express allowance in units of the user's own working time. Nobody has to reason about
percentages or ratios to read it.

```text
claude · session      1h 40m left of usage · resets in 3h
                      at this pace you run out 1h 20m early
```

The percentage stays — it is the source's own statement and the most trustworthy number on screen —
but it stops being the only thing said.

New helpers in [`format.ts`](src/format.ts), matching the existing register (lowercase, coarsest
unit that still answers the question, `UNKNOWN` for unreported):

| Helper | Returns |
| --- | --- |
| `formatRunway(minutes \| null)` | `"1h 40m"`, `"25m"`, or `UNKNOWN` |
| `describePaceState(pace)` | The one-line verdict, exhaustive switch |
| `describePaceBasis(basis)` | `"measured over the last 30m"` / `"averaged since the window opened"` |
| `describePaceConfidence(pace)` | The caveat, or `null` when it needs none |

`describePaceBasis` is not decoration. § 1.4 means a projection for Cursor's billing cycle is
materially weaker than one for Claude's session, and the interface should say which it is holding.

## 7.2 Where it appears

- **MiniView** ([`MiniView.tsx:135`](src/components/MiniView.tsx#L135)) — one line per window, under
  the existing `mini-detail` row. This is the surface the feature is for.
- **Dashboard header** — the existing quota chip ([`App.tsx:62`](src/App.tsx#L62)) gains the runway.
- **`SourceStrip`** — per-source state.
- **Menu bar** — one glyph, after § 6.4.

## 7.3 Colour

[`theme.ts`](src/theme.ts) states the invariant it is protecting: *"Nothing else in the interface is
colored, so these read as data rather than decoration."* A green/amber/red palette breaks it, and
collides concretely — Codex is already green `#6cc4a1` and Claude Code amber `#dda06a`, so a green
Codex dot beside a green "safe" badge is genuinely ambiguous.

Decided: **words and position carry the state; colour is redundancy only.** Every state is legible
with colour disabled, which is also the right answer for
[`CODE_REVIEW.md`](CODE_REVIEW.md) § F22 (keyboard focus and colour-only encoding). If state hues are
introduced at all, they need a register visibly distinct from the source hues — different saturation
and a different shape, not the same dot in a different colour.

---

# 8. Tests

The Rust side has 224 tests and it shows; a pure function with an injected `now` is the easiest thing
in the codebase to test well. Table-driven, in `domain/pace.rs`:

| Case | Asserts |
| --- | --- |
| Exactly on the line | `Amber`, `runway == T` |
| 2× over | `Red`, `shortfall == T / 2` |
| The § 1.2 three-step sequence | `Red`, `shortfall == 30`, ratio 3.0 |
| Comfortable billing cycle | `Green`, slack present |
| Idle | `Green`, `projected_exhaustion_at == None`, `runway == None` |
| Rolling window recovering (negative delta) | `Green`, basis records recovery |
| `resets_at: None` | `NotStarted`, no projection |
| `remaining == 0` | `Exhausted` |
| Reading older than the § 2.3 limit | `Unknown` |
| Samples straddling a reset | That horizon discarded, not differenced |
| `consumed < 20` tenths | That horizon discarded (quantisation floor) |
| Single reading, no samples | `SinceWindowOpen`, `assumed_anchored` recorded |
| Window > 1440 min | Short horizons **not** in the ladder |
| Integer-only | No `f32`/`f64` anywhere in the module |

Plus invariants, as property or explicit tests:

- `Unknown` and `NotStarted` never coexist with a `runway_minutes`.
- **No input produces `Green` when confidence is absent.** The single most important test here.
- `state == Red` ⟺ `shortfall_minutes.is_some()`.
- `pace_ratio_percent` and `runway_minutes` never disagree about which side of the line they are on.
- Ordering matches `quotaGroups`: tightest source first, shortest window first.

Repository tests: a sample is written only on value change; a sample and its quota commit or roll
back together; retention prunes past the horizon; a version-1 store migrates in place.

TypeScript: `formatRunway` boundaries and the exhaustive switches. Blocked on
[`CODE_REVIEW.md`](CODE_REVIEW.md) § F28 — there is no Vitest harness yet. Worth adding here, since
this is the first frontend logic in the app with arithmetic in it.

---

# 9. Phases

## Phase A — the risk state, before F1. No schema change.

Ships the feature on the `SinceWindowOpen` rung alone. No new scans of `records`, no migration, no
adapter changes — so it does not touch the read path F1 is about to rewrite.

1. `src-tauri/src/domain/pace.rs`: `PaceState`, `PaceBasis`, `WindowPace`, `evaluate_pace`, with the
   § 1.4 anchored caveat recorded in the basis.
2. The § 8 test table for every case reachable without samples.
3. `assemble_snapshot` computes it; `DashboardSnapshot.pace` carries it; both backends.
4. `src/domain/pace.ts` mirror; `DashboardSnapshot.pace` in `src/domain/usage.ts`.
5. `format.ts` helpers, exhaustive switches.
6. MiniView line and dashboard header chip.
7. F11's `useNow()` ticker, since the countdown needs it.

*Done when: Claude's 5-hour session shows a runway and a state that changes as usage lands, with the
basis stated, and no new full-table scan appears in the snapshot path.*

## Phase B — real pace, from samples.

8. The ordered migration runner (§ 5.3), `SCHEMA_VERSION` 1 → 2. Shared with F1.
9. `quota_samples` table; insert-on-change inside `apply()`; `UsageReader::quota_samples`.
10. The horizon ladder (§ 4.1) and the worst-of rule; live-quota-as-right-endpoint (§ 4.2).
11. Retention pruning on the idle path, beside F5's checkpoint.
12. Sample tests: instance segmentation, change-only writes, transactional atomicity, pruning.

*Done when: a burst inside a session moves the state within a minute, and a pause returns it without
flapping.*

## Phase C — the historical baseline, after F1.

Needs F1's indexed read path and the rollup table F6's retention contract wants anyway. Two things
it delivers:

13. Hourly rollups of `display_total` and cost, bucketed by **local** hour — records store UTC
    milliseconds, so this needs a zone shift and an explicit DST decision.
14. Baseline as the **median** per hour-of-day over a trailing N days. Median, not mean: this store
    took 48,796 records in about two days, mostly Codex, with idle nights — a mean over wall-clock
    time averages in sleep and is meaningless. Minimum-history rule (proposed: 7 days, and a minimum
    count of non-zero observations per slot) before a baseline is allowed to exist at all.
15. `paceVsBaseline`: `below | typical | above | farAbove`, as *explanation* — "2.4× your usual
    Tuesday afternoon" — never as the state.
16. **The duty cycle**, which is what § 1.4 is waiting for: with typical active hours per day known,
    a weekly or monthly window can finally be projected properly, and the short horizons can be
    enabled for long windows.

*Done when: a long window's projection accounts for the fact that the user sleeps.*

## Phase D — the payoff.

17. `ThresholdMetric::ProjectedExhaustion`
    ([`options.rs:14`](src-tauri/src/domain/options.rs#L14)), `value` = minimum shortfall in minutes
    worth warning about, default 30. Fires with hours of lead time instead of at a fixed percentage.
18. Reuses the entire existing pipeline unchanged: notification with actions, in-app banner, snooze
    ledger, `HANDOFF_PROMPT`.

Touch list for the new metric, all of which fail loudly except the last two:
`AppOptions::validate` and `normalized` ([`options.rs:76`](src-tauri/src/domain/options.rs#L76)),
the camelCase round-trip test asserting `"remaining_percent"`
([`options.rs:211`](src-tauri/src/domain/options.rs#L211)), `ThresholdMetric::ALL`,
`ThresholdAlert::body`, `src/domain/options.ts`, and `SettingsView.tsx`.

---

# 10. Open questions

1. **Local-hour bucketing and DST.** Records are UTC milliseconds; activity is diurnal in local
   time. Shift in SQL with a fixed offset, or aggregate by UTC hour and fold in Rust with the
   machine's zone? The second is more correct across a DST boundary and more work. Phase C.
2. **Baseline slot granularity.** Hour-of-day alone, or hour-of-day × weekday? The latter is four
   times more honest and needs four times the history before it says anything.
3. **Cost as a fourth axis.** Records already carry billed cost for Cursor, and Claude's
   `five_hour` window carries a `limit_dollars` field (null on this plan). A money runway —
   "$4.20/day, $31 left" — is the same arithmetic on a different unit. Deferred, not dismissed.
4. **Per-model pace.** Requested in the original framing. Only possible in percent where the source
   meters per model — Claude's Opus/Sonnet weekly caps do, and they already arrive as separate
   labelled pools, so they get a `WindowPace` each for free. Per-model pace in *tokens* is available
   from records for every source, but tokens are not the unit anything is metered in (§ 1), so it
   belongs to the Phase C activity story, not the risk story.
5. **Are the band constants right?** `T/10`, the 20-tenth quantisation floor, `window/10` and
   `window/40` horizons, the 15-minute staleness floor. All defensible, none validated. They live in
   one place as named constants and want tuning against a week of real readings.

---

# 11. Invariants this must not break

From [`CODE_REVIEW.md`](CODE_REVIEW.md) § What is already good — please don't lose it. Each of these
is a way this feature could go wrong:

- **Single-owner refresh.** The pace domain reads. It starts no timer, no poll, no watcher. The
  quota lane already runs at the right cadence and `refresh.rs` stays its only owner.
- **The revisioned snapshot contract.** Pace travels *in* the snapshot (§ 6.2). Never a second fetch.
- **Integer-only percentages.** No float reaches this calculation. Runway is whole minutes, the ratio
  is whole percent, and the one division happens last (§ 1.1).
- **`null` means unreported, never zero.** `runway_minutes: None` is "no pace measured" and must
  never render as `0m`. This is the same discipline `TokenField.quality` already carries.
- **Two freshness clocks stay two.** The pace rests on `observed_at`, the source's own statement,
  not on when the app read it. § 2.3 exists because conflating them would make a stale reading look
  safe.
- **Comments explain why, not what.** Every non-obvious constant in § 2 and § 4 carries the reason
  it is that value, because none of them is derivable from the code.
