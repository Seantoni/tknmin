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
  /** Identity: which source event this is. Stable across corrections. */
  dedupeKey: string;
  dedupeAlgorithmVersion: number;
  /** Content: what this observation of the event said. */
  contentHash: string;
  contentHashVersion: number;
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

/**
 * Static facts about a source application.
 *
 * Deliberately says nothing about whether the source is currently readable:
 * that is live state, it changes without the interface asking, and it travels
 * in the snapshot as {@link SourceSyncHealth}.
 */
export interface AdapterInfo {
  id: string;
  version: string;
  sourceApp: SourceApp;
  displayName: string;
  /** True when reading this source can require the network. */
  needsNetwork: boolean;
}

/** What a source's last synchronization attempt did. */
export type SyncState =
  | "unknown"
  | "current"
  | "syncing"
  | "stale"
  | "offline"
  | "error";

/**
 * One source's freshness, as committed.
 *
 * Two clocks, kept apart on purpose. `sourceObservedAt` is when the source
 * says its data was true; `appSyncedAt` is when this app last read it. A
 * successful read of a stale file advances the second and not the first.
 */
export interface SourceSyncHealth {
  sourceApp: SourceApp;
  state: SyncState;
  appSyncedAt: string | null;
  sourceObservedAt: string | null;
  lastAttemptAt: string | null;
  lastError: string | null;
  /**
   * Activity was detected but the provider has not published the billed
   * numbers for it yet. Cursor does this routinely.
   */
  awaitingUpstream: boolean;
}

/** Which half of a source's data a change touched. */
export type RefreshCategory = "usage" | "quota";

/**
 * The one event the backend emits when data changes.
 *
 * It always names a revision that is already durable, so refetching on it can
 * never read a revision that does not exist yet.
 */
export interface DataChanged {
  revision: number;
  affectedSources: SourceApp[];
  affectedCategories: RefreshCategory[];
  inserted: number;
  updated: number;
  deleted: number;
  appSyncedAt: string;
}

/**
 * Everything one screen renders, read at a single revision.
 *
 * Fetched as a unit precisely so a total from one revision can never appear
 * beside a quota from another.
 */
export interface DashboardSnapshot {
  revision: number;
  /** Unfiltered, so proportions stay comparable while a filter is active. */
  overview: UsageSummary;
  /** Reflects the active filter. */
  summary: UsageSummary;
  recent: UsageRecord[];
  recordCount: number;
  /**
   * Exactly what the sources reported, never a derived value. Any matching
   * entry in {@link DashboardSnapshot.projections} carries the inferred part.
   */
  quotas: UsageQuota[];
  health: SourceSyncHealth[];
  /**
   * One pace row per live allowance window, computed from the quotas in this
   * same snapshot so a projection never sits beside a quota from a different
   * revision than the one that produced it.
   */
  pace: import("./pace").WindowPace[];
  /**
   * Confirmed readings carried forward to this snapshot's instant, for the
   * windows where a trustworthy rate could be fitted. Absent means the
   * confirmed number in {@link DashboardSnapshot.quotas} stands alone.
   */
  projections: import("./pace").QuotaProjection[];
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
  /**
   * RFC 3339 instant the window resets, or null when no window is running — a
   * rolling window that nothing has started yet, with its allowance untouched.
   */
  resetsAt: string | null;
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
