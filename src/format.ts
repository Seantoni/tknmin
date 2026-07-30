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
import type { WindowPace } from "./domain/pace";

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
 * Sources come in order of how close they are to running out, and a source's own
 * windows come shortest-first — the session before the week that contains it.
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
    .sort((left, right) => tightestRemaining(left.windows) - tightestRemaining(right.windows));
}

function tightestRemaining(quotas: UsageQuota[]): number {
  return Math.min(...quotas.map(quotaRemainingTenths));
}

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
export function describeSourceFreshness(health: SourceSyncHealth): string {
  const lines = [
    `${describeSyncState(health)}`,
    `app synced ${formatAge(health.appSyncedAt)}`,
    `source reported ${formatAge(health.sourceObservedAt)}`,
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
