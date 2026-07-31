/**
 * Presentation helpers.
 *
 * Two rules run through all of them: a value the source never reported is
 * shown as unknown rather than as zero, and money is never converted to a
 * float — rounding happens in integer arithmetic.
 */

import type {
  FieldQuality,
  Money,
  SourceApp,
  SourceSyncHealth,
  TimestampInterpretation,
  TokenField,
  TokenTotal,
  CostTotal,
  TotalRule,
  UsageQuota,
  UsageRecord,
} from "./domain/usage";
import type { QuotaProjection, WindowPace } from "./domain/pace";

/** Shown wherever a source reported nothing. */
export const UNKNOWN = "—";

const numberFormat = new Intl.NumberFormat();

const dateTimeFormat = new Intl.DateTimeFormat(undefined, {
  dateStyle: "medium",
  timeStyle: "short",
});

const shortDateFormat = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
});

const shortTimeFormat = new Intl.DateTimeFormat(undefined, {
  hour: "2-digit",
  minute: "2-digit",
  hour12: false,
});

export function formatTokens(value: number): string {
  return numberFormat.format(value);
}

/** A token count, or the unknown marker when the source omitted it. */
export function formatTokenField(field: TokenField): string {
  return field.value === null ? UNKNOWN : formatTokens(field.value);
}

/** A summed category. The sum covers only the records that reported it. */
export function formatTokenTotal(total: TokenTotal): string {
  return formatTokens(total.tokens);
}

/**
 * Explains what a total leaves out, or null when it is complete. The interface
 * shows this rather than implying every record contributed.
 */
export function describeTotalGaps(total: TokenTotal): string | null {
  if (total.unknownRecords === 0) return null;
  const records = total.unknownRecords === 1 ? "record" : "records";
  return `${formatTokens(total.unknownRecords)} ${records} did not report this`;
}

/**
 * Round an exact minor-unit amount to a fixed number of decimals using integer
 * arithmetic, then render it. No floating point touches the value.
 */
export function formatMoneyAmount(money: Money, fractionDigits = 2): string {
  const { amountMinor, currency, minorUnitExponent } = money;
  const sign = amountMinor < 0 ? "-" : "";
  const magnitude = Math.abs(amountMinor);

  let scaled: number;
  if (minorUnitExponent > fractionDigits) {
    const divisor = 10 ** (minorUnitExponent - fractionDigits);
    const quotient = Math.trunc(magnitude / divisor);
    const remainder = magnitude % divisor;
    // Half-up, decided by comparing integers rather than by rounding a float.
    scaled = remainder * 2 >= divisor ? quotient + 1 : quotient;
  } else {
    scaled = magnitude * 10 ** (fractionDigits - minorUnitExponent);
  }

  if (fractionDigits === 0) {
    return `${sign}${numberFormat.format(scaled)} ${currency}`;
  }

  const digits = scaled.toString().padStart(fractionDigits + 1, "0");
  const whole = Number(digits.slice(0, digits.length - fractionDigits));
  const fraction = digits.slice(digits.length - fractionDigits);
  return `${sign}${numberFormat.format(whole)}.${fraction} ${currency}`;
}

/** Every currency in a cost total, kept separate because they cannot be added. */
export function formatCostTotal(cost: CostTotal): string {
  if (cost.byCurrency.length === 0) return UNKNOWN;
  return cost.byCurrency.map((entry) => formatMoneyAmount(entry.amount)).join(" · ");
}

export function describeCostGaps(cost: CostTotal): string | null {
  if (cost.recordsWithoutCost === 0) return null;
  const records = cost.recordsWithoutCost === 1 ? "record" : "records";
  return `${formatTokens(cost.recordsWithoutCost)} ${records} reported no cost`;
}

/**
 * When the event happened, in this Mac's timezone. Falls back to the raw
 * source string when the timestamp could not be resolved, so the row still
 * shows what the log actually said.
 */
export function formatEventTime(record: UsageRecord): string {
  if (record.eventTimestampUtc === null) {
    return record.rawTimestamp ?? UNKNOWN;
  }
  return dateTimeFormat.format(new Date(record.eventTimestampUtc));
}

/**
 * The event time in a dense row: "Jul 29 09:33".
 *
 * Built from two formatters rather than one combined one, whose locale glue
 * ("Jul 29 at 09:33") is too wide for a single-line column.
 */
export function formatShortEventTime(record: UsageRecord): string {
  if (record.eventTimestampUtc === null) {
    return record.rawTimestamp ?? UNKNOWN;
  }
  const instant = new Date(record.eventTimestampUtc);
  return `${shortDateFormat.format(instant)} ${shortTimeFormat.format(instant)}`;
}

/** The span the records cover, or null when none carries a usable date. */
export function formatDateSpan(records: UsageRecord[]): string | null {
  const times = records
    .map((record) => record.eventTimestampUtc)
    .filter((value): value is string => value !== null)
    .map((value) => new Date(value).getTime());

  if (times.length === 0) return null;

  const first = shortDateFormat.format(new Date(Math.min(...times)));
  const last = shortDateFormat.format(new Date(Math.max(...times)));
  return first === last ? first : `${first} – ${last}`;
}

/**
 * The span between two instants as dates: "Jul 30", or "Jul 29 – Jul 30"
 * when they cross midnight. `null` when neither instant exists.
 */
export function formatInstantSpan(
  first: string | null,
  last: string | null,
): string | null {
  const start = first ?? last;
  const end = last ?? first;
  if (start === null || end === null) return null;

  const a = shortDateFormat.format(new Date(start));
  const b = shortDateFormat.format(new Date(end));
  return a === b ? a : `${a} – ${b}`;
}

/** A share of a whole, rounded to whole percent. Zero totals yield zero. */
export function share(value: number, total: number): number {
  if (total <= 0) return 0;
  return (value / total) * 100;
}

export function formatPercent(value: number): string {
  return `${Math.round(value)}%`;
}

/** "6.5%" or "7%" from integer tenths of a percent — no floats, like money. */
export function formatPercentTenths(tenths: number): string {
  const whole = Math.floor(tenths / 10);
  const tenth = tenths % 10;
  return tenth === 0 ? `${whole}%` : `${whole}.${tenth}%`;
}

/** Tenths of a percent of the window still unused, never below zero. */
export function quotaRemainingTenths(quota: UsageQuota): number {
  return Math.max(0, 1000 - quota.usedPercentTenths);
}

/**
 * Where this window's usage would sit right now if it were spent evenly — the
 * "should be" line, as a percent used.
 *
 * Even pace is the rate that lands on 100% exactly as the window resets, so a
 * fill short of this mark has room and a fill past it is borrowing from later.
 * It is the same assumption the pace domain's window-open measurement makes:
 * that the window opened `windowMinutes` before it resets. For a rolling
 * window that is an approximation, and one that errs toward caution, since a
 * rolling allowance also returns as it ages.
 *
 * `null` before there is anything to compare against — a window with no reset
 * has not started, and one that has only just opened has no elapsed share yet.
 */
export function quotaEvenPacePercent(quota: UsageQuota, now: Date = new Date()): number | null {
  if (quota.windowMinutes <= 0) return null;
  const remaining = minutesUntil(quota.resetsAt, now);
  if (remaining === null) return null;
  const elapsed = quota.windowMinutes - remaining;
  if (elapsed <= 0) return null;
  return Math.min(100, (elapsed / quota.windowMinutes) * 100);
}

/** The same mark in words, for the row's tooltip. */
export function describeEvenPace(quota: UsageQuota, now: Date = new Date()): string | null {
  const target = quotaEvenPacePercent(quota, now);
  if (target === null) return null;
  return `even pace would be ${formatPercent(target)} used by now`;
}

/** The quota window the way the user thinks of it: "this week", "today". */
export function describeQuotaWindow(windowMinutes: number): string {
  const WEEK = 10080;
  const DAY = 1440;
  const SESSION = 300;
  if (windowMinutes === SESSION) return "this session";
  if (windowMinutes === WEEK) return "this week";
  if (windowMinutes === DAY) return "today";
  if (windowMinutes > DAY && windowMinutes % DAY === 0) {
    return `this ${windowMinutes / DAY}d period`;
  }
  if (windowMinutes % 60 === 0) return `this ${windowMinutes / 60}h window`;
  return `this ${windowMinutes}min window`;
}

/**
 * One quota per source: the window with the least left.
 *
 * A plan can meter several windows at once — Claude runs a 5-hour session
 * inside a 7-day week — and the one closest to running out is the one that
 * decides whether the next request goes through. The rest stay available in
 * the chip's tooltip rather than crowding the header.
 */
export function tightestQuotaPerSource(quotas: UsageQuota[]): UsageQuota[] {
  const tightest = new Map<string, UsageQuota>();
  for (const quota of quotas) {
    const held = tightest.get(quota.sourceApp);
    if (held === undefined || quotaRemainingTenths(quota) < quotaRemainingTenths(held)) {
      tightest.set(quota.sourceApp, quota);
    }
  }
  return [...tightest.values()];
}

/** Every window one source meters, kept together under that source. */
export interface QuotaGroup {
  sourceApp: SourceApp;
  windows: UsageQuota[];
}

/**
 * Every allowance window there is, grouped by the source it belongs to rather
 * than collapsed to one number per source.
 *
 * The header chip and the menu bar have room for a single number and so show the
 * window that binds first. A list has room for all of them, and they are not
 * interchangeable: Claude's 5-hour session and its 7-day week run out on their
 * own schedules, its per-model caps are separate pools inside the week, and
 * Cursor's own models cannot spend what is left of the others. Grouping is what
 * keeps those readable — two lines about Cursor have to look like Cursor's.
 *
 * Sources always come in the same order — codex, then claude, then cursor —
 * because a glanceable panel earns its keep when every source keeps its own
 * spot; an order that moves with the numbers has to be re-read every time. A
 * source's own windows come shortest-first — the session before the week
 * that contains it.
 */
export function quotaGroups(quotas: UsageQuota[]): QuotaGroup[] {
  const groups = new Map<SourceApp, UsageQuota[]>();
  for (const quota of quotas) {
    const windows = groups.get(quota.sourceApp);
    if (windows === undefined) groups.set(quota.sourceApp, [quota]);
    else windows.push(quota);
  }

  for (const windows of groups.values()) {
    windows.sort((left, right) => {
      if (left.windowMinutes !== right.windowMinutes) {
        return left.windowMinutes - right.windowMinutes;
      }
      return (left.label ?? "").localeCompare(right.label ?? "");
    });
  }

  return [...groups.entries()]
    .map(([sourceApp, windows]) => ({ sourceApp, windows }))
    .sort((left, right) => SOURCE_ORDER[left.sourceApp] - SOURCE_ORDER[right.sourceApp]);
}

/** The fixed reading order of the sources, first to last. */
const SOURCE_ORDER: Record<SourceApp, number> = {
  codex: 0,
  claude_code: 1,
  cursor: 2,
};

/** Identifies one window of one pool: a source, its label, and its length. */
export function quotaRowKey(quota: UsageQuota): string {
  return `${quota.sourceApp}:${quota.label ?? ""}:${quota.windowMinutes}`;
}

/**
 * The window in the fewest characters that still name it: "5h", "week", "31d".
 * Used where the window has to sit beside the pool it belongs to, and where the
 * window is all a row has to be called.
 */
export function quotaWindowTag(windowMinutes: number): string {
  const WEEK = 10080;
  const DAY = 1440;
  if (windowMinutes === WEEK) return "week";
  if (windowMinutes === DAY) return "day";
  if (windowMinutes > DAY && windowMinutes % DAY === 0) return `${windowMinutes / DAY}d`;
  if (windowMinutes % 60 === 0) return `${windowMinutes / 60}h`;
  return `${windowMinutes}min`;
}

/** Every window a source meters, one per line, for a chip's tooltip. */
export function describeQuotaWindows(quotas: UsageQuota[]): string {
  const lines = quotas
    .slice()
    .sort((left, right) => left.windowMinutes - right.windowMinutes)
    .map(
      (quota) =>
        `${quota.label === null ? "" : `${quota.label}: `}${formatPercentTenths(
          quotaRemainingTenths(quota),
        )} left ${describeQuotaWindow(quota.windowMinutes)} · ${describeQuotaReset(
          quota.resetsAt,
        )}`,
    );
  const observed = quotas[0]?.observedAt;
  if (observed !== undefined) {
    lines.push(`reported by the source ${formatQuotaObserved(observed)}`);
  }
  return lines.join("\n");
}

/** When the window resets: "Jul 28 14:54". */
export function formatQuotaReset(resetsAt: string): string {
  const instant = new Date(resetsAt);
  return `${shortDateFormat.format(instant)} ${shortTimeFormat.format(instant)}`;
}

/**
 * When the window resets, or that it has not started — the two things a reset
 * time can say. Phrased as a whole clause because both readings have to fit the
 * same slot in a row.
 */
export function describeQuotaReset(resetsAt: string | null): string {
  return resetsAt === null ? NOT_STARTED : `resets ${formatQuotaReset(resetsAt)}`;
}

/**
 * The same, in as few characters as the compact window has room for: the time
 * alone when the window resets today, the date as well when it does not.
 */
export function describeQuotaResetCompact(resetsAt: string | null): string {
  if (resetsAt === null) return NOT_STARTED;
  const instant = new Date(resetsAt);
  if (instant.toDateString() === new Date().toDateString()) {
    return `resets ${shortTimeFormat.format(instant)}`;
  }
  return `resets ${formatQuotaReset(resetsAt)}`;
}

/** Said of a rolling window nothing has started: the allowance is all there. */
const NOT_STARTED = "not started";

/** How stale the snapshot is, for the tooltip: "reported Jul 28 12:00". */
export function formatQuotaObserved(observedAt: string): string {
  const instant = new Date(observedAt);
  return `${shortDateFormat.format(instant)} ${shortTimeFormat.format(instant)}`;
}

/**
 * How long ago something happened, in the coarsest unit that still answers
 * the question: "just now", "40s ago", "6m ago", "3h ago", "2d ago".
 *
 * Precision beyond that would be false confidence — nothing on this screen
 * turns on the difference between 3 and 4 hours old.
 */
export function formatAge(instant: string | null, now: Date = new Date()): string {
  if (instant === null) return "never";
  const elapsed = now.getTime() - new Date(instant).getTime();
  if (!Number.isFinite(elapsed)) return UNKNOWN;
  if (elapsed < 0) return "just now";

  const seconds = Math.floor(elapsed / 1000);
  if (seconds < 10) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

/**
 * The one-word state of a source, as the interface says it.
 *
 * "watching" rather than "current" for a healthy local source, because that is
 * the honest description: nothing is being polled, a watcher is in place, and
 * the number is as fresh as the last thing the source wrote.
 */
export function describeSyncState(health: SourceSyncHealth): string {
  switch (health.state) {
    case "syncing":
      return "syncing";
    case "current":
      return health.awaitingUpstream ? "awaiting billing" : "watching";
    case "stale":
      return "stale";
    case "offline":
      return "offline";
    case "error":
      return "error";
    case "unknown":
      return "not checked";
  }
}

/**
 * The full freshness story for a source, for a tooltip.
 *
 * Both clocks appear, because they answer different questions and routinely
 * disagree: Cursor publishes billing hours after the activity that caused it,
 * so "app synced 5s ago" and "source reported 3h ago" are both true and only
 * the pair of them is honest.
 */
export function describeSourceFreshness(
  health: SourceSyncHealth,
  now: Date = new Date(),
): string {
  const lines = [
    `${describeSyncState(health)}`,
    `app synced ${formatAge(health.appSyncedAt, now)}`,
    `source reported ${formatAge(health.sourceObservedAt, now)}`,
  ];
  if (health.awaitingUpstream) {
    lines.push("activity detected; awaiting billing data");
  }
  if (health.lastError !== null) {
    lines.push(`last error: ${health.lastError}`);
  }
  return lines.join(" · ");
}

/** A caveat about how a timestamp was read, or null when it needs none. */
export function describeTimestamp(interpretation: TimestampInterpretation): string | null {
  switch (interpretation) {
    case "assumed_utc":
      return "no timezone in the log; read as UTC";
    case "assumed_local":
      return "no timezone in the log; read as local time";
    case "unparsable":
      return "the log's timestamp could not be read";
    case "missing":
      return "the log recorded no timestamp";
    case "explicit_offset":
      return null;
  }
}

/** A caveat about a token count, or null when it was reported exactly. */
export function describeQuality(quality: FieldQuality): string | null {
  switch (quality) {
    case "estimated":
      return "estimated by the source";
    case "partial":
      return "the source reported only part of this";
    case "unknown":
      return "not reported";
    case "exact":
      return null;
  }
}

/** How a record's total was arrived at. */
export function describeTotalRule(rule: TotalRule): string {
  switch (rule) {
    case "reported_by_source":
      return "total reported by the source";
    case "input_plus_output":
      return "input + output";
    case "input_plus_output_plus_reasoning":
      return "input + output + reasoning";
  }
}

export function orUnknown(value: string | null): string {
  return value === null || value === "" ? UNKNOWN : value;
}

/* ------------------------------------------------------------------ */
/* Pace                                                                */
/* ------------------------------------------------------------------ */

/**
 * A length of usage time in the coarsest unit that still answers the
 * question: "25m", "1h 40m", "2d 3h". `null` is unmeasured, never zero.
 */
export function formatRunway(minutes: number | null): string {
  if (minutes === null) return UNKNOWN;
  if (minutes < 1) return "<1m";
  if (minutes < 60) return `${Math.round(minutes)}m`;
  const hours = Math.floor(minutes / 60);
  const rest = Math.round(minutes % 60);
  if (hours < 24) {
    return rest === 0 ? `${hours}h` : `${hours}h ${rest}m`;
  }
  const days = Math.floor(hours / 24);
  const restHours = hours % 24;
  return restHours === 0 ? `${days}d` : `${days}d ${restHours}h`;
}

/** Minutes between now and a future instant, never negative. */
export function minutesUntil(isoInstant: string | null, now: Date = new Date()): number | null {
  if (isoInstant === null) return null;
  const ms = new Date(isoInstant).getTime() - now.getTime();
  if (!Number.isFinite(ms)) return null;
  return Math.max(0, Math.round(ms / 60_000));
}

/**
 * The one-line pace verdict, as the interface says it. Exhaustive on purpose:
 * a new state variant must fail `tsc` here rather than render blank.
 */
export function describePaceState(pace: WindowPace, now: Date = new Date()): string {
  const runway = formatRunway(pace.runwayMinutes);
  switch (pace.state) {
    case "notStarted":
      return "allowance untouched";
    case "green":
      return pace.runwayMinutes === null
        ? "nothing being spent"
        : `${runway} of usage left · comfortably inside the window`;
    case "amber":
      return `${runway} of usage left · right at the edge of the window`;
    case "red": {
      void now;
      const shortfall = formatRunway(pace.shortfallMinutes);
      return `runs out ${shortfall} early at this pace`;
    }
    case "exhausted":
      return "allowance used up";
    case "unknown":
      return "too early to say";
  }
}

/**
 * The pace verdict in the fewest words that can still be acted on.
 *
 * The compact window is a few hundred pixels wide, so a sentence there wraps
 * to three ragged lines and stops being scannable. What survives the cut is
 * the one fact that changes behaviour: for a window heading for trouble, when
 * it runs dry — an absolute duration the eye can compare against the reset
 * time sitting next to it. Everything else is reassurance, and reassurance
 * only needs two words.
 *
 * `null` where the row's reset time already says it, so the two never repeat
 * each other.
 */
export function describePaceCompact(pace: WindowPace): string | null {
  switch (pace.state) {
    case "red":
      return `out in ${formatRunway(pace.runwayMinutes)}`;
    case "amber":
      return "at the edge";
    case "green":
      return pace.runwayMinutes === null ? "idle" : "on track";
    case "exhausted":
      return "used up";
    // "not started" is already what the reset slot reads, and "too early to
    // say" is noise next to a percentage that is plainly untouched.
    case "notStarted":
    case "unknown":
      return null;
  }
}

/**
 * What the verdict is measured against, so "out in 19d" is not left hanging.
 *
 * A duration alone cannot be judged: 19 days sounds generous until you know
 * the window has 24 to go. This supplies the missing half of the comparison —
 * how far short of the reset the allowance lands — which is the number that
 * says whether to change anything.
 *
 * `null` where there is no gap worth naming, so a healthy row stays quiet.
 */
export function describePaceGap(pace: WindowPace): string | null {
  if (pace.state === "red" && pace.shortfallMinutes !== null) {
    return `${formatRunway(pace.shortfallMinutes)} short`;
  }
  return null;
}

/**
 * The pace row matching one quota window, when one was computed. The join is
 * on all three parts of the window's identity, because a source can meter
 * several pools at once and they are not interchangeable.
 */
export function paceForQuota(paces: WindowPace[], quota: UsageQuota): WindowPace | undefined {
  return paces.find(
    (each) =>
      each.sourceApp === quota.sourceApp &&
      each.windowMinutes === quota.windowMinutes &&
      each.label === quota.label,
  );
}

/**
 * The projection for one row, when the backend earned one. Undefined is the
 * ordinary case, not a failure: it means the confirmed reading is the best
 * available statement and should be shown exactly as reported.
 */
export function projectionForQuota(
  projections: QuotaProjection[],
  quota: UsageQuota,
): QuotaProjection | undefined {
  return projections.find(
    (each) =>
      each.sourceApp === quota.sourceApp &&
      each.windowMinutes === quota.windowMinutes &&
      each.label === quota.label,
  );
}

/**
 * Tenths of the window consumed as currently believed — the projection when
 * there is one, the confirmed reading otherwise.
 *
 * This is what the headline and the bar are drawn from, because the question a
 * row answers is "how much is left *now*", and a reading published ten minutes
 * ago answers a different one. Provenance is not lost: the caller marks a
 * projected row, and [`describeProjection`] spells the split out.
 */
export function quotaUsedTenths(
  quota: UsageQuota,
  projection: QuotaProjection | undefined,
): number {
  return projection?.projectedPercentTenths ?? quota.usedPercentTenths;
}

/** Tenths still unused as currently believed, never below zero. */
export function quotaLiveRemainingTenths(
  quota: UsageQuota,
  projection: QuotaProjection | undefined,
): number {
  return Math.max(0, 1000 - quotaUsedTenths(quota, projection));
}

/**
 * Where a projected number came from, for the tooltip — the measured part, its
 * age, the derived increment, and how well the rate behind it is established.
 * A user who wants to know whether to believe the "≈" gets the whole basis.
 */
export function describeProjection(
  projection: QuotaProjection,
  now: Date = new Date(),
): string {
  return [
    `${formatPercentTenths(projection.confirmedPercentTenths)} confirmed ${formatAge(
      projection.confirmedAt,
      now,
    )}`,
    `+${formatPercentTenths(projection.addedPercentTenths)} estimated from usage since`,
    `rate fitted from ${projection.pairs} readings, spread ±${projection.residualPercent}%`,
  ].join("\n");
}

/**
 * The whole pace story, for a tooltip: the verdict as a sentence, then the
 * caveats that qualify it. These are what the compact row deliberately leaves
 * out, so the detail stays one hover away rather than gone.
 */
export function describePaceDetail(pace: WindowPace, now: Date = new Date()): string {
  const lines = [describePaceState(pace, now)];
  const baseline = describeVsBaseline(pace);
  if (baseline !== null) lines.push(baseline);
  const basis = describePaceBasis(pace);
  if (basis !== null) lines.push(basis);
  const age = describePaceReadingAge(pace, now);
  if (age !== null) lines.push(age);
  return lines.join("\n");
}

/**
 * Said only when the reading behind the verdict is older than the window would
 * normally tolerate — Codex publishes its allowance only while it runs, so
 * between sessions this is the ordinary case rather than a fault.
 *
 * The verdict is still worth showing: the records agree nothing has been spent
 * since, so an old reading is an exact one. But if that ever stops being true
 * the error runs one way — an unrecorded burn leaves less than the number
 * says — so the direction is named rather than left to be guessed.
 */
export function describePaceReadingAge(
  pace: WindowPace,
  now: Date = new Date(),
): string | null {
  if (!pace.fromAgedReading) return null;
  return `from a reading ${formatAge(pace.observedAt, now)}; nothing spent since, so it can only be optimistic`;
}

/**
 * How the current burn compares to this source's own history at this hour, as
 * a short qualifier. `noBaseline` and `typical` say nothing — only a departure
 * from the user's own normal is worth the space.
 */
export function describeVsBaseline(pace: WindowPace): string | null {
  switch (pace.vsBaseline) {
    case "above":
      return "heavier than your usual right now";
    case "farAbove":
      return "far heavier than your usual right now";
    case "below":
      return "lighter than your usual right now";
    case "typical":
    case "noBaseline":
      return null;
  }
}

/** How the pace was measured, so a weak projection says so. */
export function describePaceBasis(pace: WindowPace): string | null {
  if (pace.basis === null) return null;
  switch (pace.basis.kind) {
    case "trailing":
      return `measured over the last ${pace.basis.minutes}m`;
    case "sinceWindowOpen":
      return pace.basis.assumedAnchored
        ? "averaged since the window opened"
        : "averaged since the window opened (a rolling window can only be safer)";
  }
}
