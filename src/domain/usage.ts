/**
 * The usage vocabulary, mirroring `src-tauri/src/domain`.
 *
 * These types describe what Rust sends over the Tauri boundary. Change them
 * only alongside the Rust definitions they mirror.
 */

export type SourceApp = "cursor" | "claude_code" | "codex";

/** How much to trust a single token count. */
export type FieldQuality = "exact" | "estimated" | "partial" | "unknown";

/** How the normalized UTC timestamp was derived from the raw source value. */
export type TimestampInterpretation =
  | "explicit_offset"
  | "assumed_utc"
  | "assumed_local"
  | "unparsable"
  | "missing";

export type CostCalculationStatus =
  | "reported_by_source"
  | "computed_from_pricing"
  | "not_available";

/** How a record's display total was arrived at. */
export type TotalRule =
  | "reported_by_source"
  | "input_plus_output"
  | "input_plus_output_plus_reasoning";

/**
 * An exact monetary amount in integer minor units. Never convert this to a
 * float; use `formatMoney`.
 */
export interface Money {
  amountMinor: number;
  /** Uppercase ISO 4217 code. */
  currency: string;
  minorUnitExponent: number;
}

/** One token category. A `null` value means unreported, never zero. */
export interface TokenField {
  value: number | null;
  quality: FieldQuality;
}

export interface TokenCounts {
  input: TokenField;
  output: TokenField;
  cachedInput: TokenField;
  reasoning: TokenField;
}

export interface DisplayTotal {
  tokens: number;
  rule: TotalRule;
}

export interface CostInfo {
  amount: Money | null;
  status: CostCalculationStatus;
  pricingVersion: string | null;
}

/** Non-sensitive provenance: never a filesystem path, prompt, or response. */
export interface SourceProvenance {
  adapterId: string;
  adapterVersion: string;
  sourceRef: string | null;
}

export interface UsageRecord {
  normalizationVersion: number;
  id: string;
  rawTimestamp: string | null;
  /** RFC 3339 instant, or null when it could not be resolved. */
  eventTimestampUtc: string | null;
  timestampInterpretation: TimestampInterpretation;
  sourceApp: SourceApp;
  sourceEventId: string | null;
  dedupeKey: string;
  dedupeAlgorithmVersion: number;
  provider: string | null;
  model: string | null;
  tokens: TokenCounts;
  reportedTotalTokens: number | null;
  displayTotal: DisplayTotal | null;
  project: string | null;
  sessionId: string | null;
  cost: CostInfo;
  importedAt: string;
  provenance: SourceProvenance;
}

/**
 * A summed token category. `tokens` covers only `countedRecords`; when
 * `unknownRecords` is above zero the sum is a lower bound.
 */
export interface TokenTotal {
  tokens: number;
  countedRecords: number;
  unknownRecords: number;
}

export interface CurrencyTotal {
  amount: Money;
  countedRecords: number;
}

export interface CostTotal {
  byCurrency: CurrencyTotal[];
  recordsWithoutCost: number;
}

export interface UsageTotals {
  recordCount: number;
  input: TokenTotal;
  output: TokenTotal;
  cachedInput: TokenTotal;
  reasoning: TokenTotal;
  displayTotal: TokenTotal;
  cost: CostTotal;
}

export interface GroupSummary {
  /** null identifies the group whose dimension was unknown. */
  key: string | null;
  label: string;
  totals: UsageTotals;
}

export interface UsageSummary {
  generatedAt: string;
  totals: UsageTotals;
  bySource: GroupSummary[];
  byModel: GroupSummary[];
  /** Records a date filter had to skip because their timestamp is unresolved. */
  undatedRecordsExcluded: number;
}

export interface SummaryQuery {
  sources?: SourceApp[] | null;
  models?: string[] | null;
  /** Inclusive RFC 3339 lower bound. */
  from?: string | null;
  /** Exclusive RFC 3339 upper bound. */
  until?: string | null;
}

export interface RecentQuery {
  limit: number;
  filter: SummaryQuery;
}

/** A source application and whether this build can read its logs yet. */
export interface AdapterInfo {
  id: string;
  version: string;
  sourceApp: SourceApp;
  displayName: string;
  readsLogs: boolean;
}

export type RefreshStatus = "imported" | "planned" | "failed";

/** What one adapter did during a log refresh. */
export interface SourceRefreshReport {
  adapter: string;
  status: RefreshStatus;
  sourcesFound: number;
  sourcesFailed: number;
  draftsParsed: number;
  inserted: number;
  duplicatesSkipped: number;
  rejected: number;
  failures: string[];
}

/** The outcome of rescanning every source's logs. */
export interface RefreshReport {
  sources: SourceRefreshReport[];
  quotas: UsageQuota[];
}

/**
 * How full one allowance window was when the source last reported it. Quota
 * is account state, not usage data — it goes stale within minutes of activity.
 */
export interface UsageQuota {
  sourceApp: SourceApp;
  /** A source-specific pool, e.g. "Cursor Models" or "Other Models". */
  label: string | null;
  /** The window's length in minutes; 10080 is a week. */
  windowMinutes: number;
  /** Tenths of a percent consumed, so 930 means 93.0%. Integer, like money. */
  usedPercentTenths: number;
  /** RFC 3339 instant the window resets. */
  resetsAt: string;
  /** When the source wrote this snapshot. */
  observedAt: string;
}

export type ErrorCode = "storage" | "adapter" | "invalid_request";export interface AppError {
  code: ErrorCode;
  message: string;
}

/** True when a total omits records that never reported the category. */
export function isPartialTotal(total: TokenTotal): boolean {
  return total.unknownRecords > 0;
}

/**
 * Render a monetary amount using integer arithmetic only, so no rounding is
 * introduced between Rust's exact value and the screen.
 */
export function formatMoney(money: Money): string {
  const { amountMinor, currency, minorUnitExponent } = money;
  if (minorUnitExponent === 0) {
    return `${amountMinor} ${currency}`;
  }
  const sign = amountMinor < 0 ? "-" : "";
  const digits = Math.abs(amountMinor).toString().padStart(minorUnitExponent + 1, "0");
  const whole = digits.slice(0, digits.length - minorUnitExponent);
  const fraction = digits.slice(digits.length - minorUnitExponent);
  return `${sign}${whole}.${fraction} ${currency}`;
}
