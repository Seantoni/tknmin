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
  TimestampInterpretation,
  TokenField,
  TokenTotal,
  CostTotal,
  TotalRule,
  UsageQuota,
  UsageRecord,
} from "./domain/usage";

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

/** Every window a source meters, one per line, for a chip's tooltip. */
export function describeQuotaWindows(quotas: UsageQuota[]): string {
  const lines = quotas
    .slice()
    .sort((left, right) => left.windowMinutes - right.windowMinutes)
    .map(
      (quota) =>
        `${quota.label === null ? "" : `${quota.label}: `}${formatPercentTenths(
          quotaRemainingTenths(quota),
        )} left ${describeQuotaWindow(
          quota.windowMinutes,
        )} · resets ${formatQuotaReset(quota.resetsAt)}`,
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

/** How stale the snapshot is, for the tooltip: "reported Jul 28 12:00". */
export function formatQuotaObserved(observedAt: string): string {
  const instant = new Date(observedAt);
  return `${shortDateFormat.format(instant)} ${shortTimeFormat.format(instant)}`;
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
