/**
 * The only path from React to Rust.
 *
 * Components call these functions rather than `invoke` directly, so command
 * names, argument shapes, and error handling live in one place.
 *
 * There is no scan here, and no cadence. The interface does not decide when
 * data is read — it subscribes to committed revisions and fetches snapshots.
 * The one exception is {@link syncNow}, which submits a trigger to the
 * backend's coordinator and is recovery only: the app stays current whether or
 * not anyone ever calls it.
 */

import { invoke } from "@tauri-apps/api/core";

import type {
  AdapterInfo,
  AppError,
  DashboardSnapshot,
  ErrorCode,
  RiskSnapshot,
  RecentQuery,
  SummaryQuery,
  UsageQuota,
  UsageRecord,
  UsageSummary,
} from "../domain/usage";

const COMMANDS = {
  dashboardSnapshot: "dashboard_snapshot",
  quotaSnapshot: "quota_snapshot",
  usageSummary: "usage_summary",
  recentUsage: "recent_usage",
  usageRecordCount: "usage_record_count",
  sourceCapabilities: "source_capabilities",
  usageQuota: "usage_quota",
  syncNow: "sync_now",
  windowFocused: "window_focused",
  isSyncing: "is_syncing",
} as const;

/** The one event every window listens to. Emitted only after a commit. */
export const DATA_CHANGED = "data-changed";

export const DEFAULT_RECENT_LIMIT = 50;

/**
 * Everything a screen renders, at one revision.
 *
 * This replaces six independent commands. Six reads can interleave with a
 * commit; one cannot, which is what stops a stat card and a quota chip from
 * describing different moments.
 */
export function fetchDashboardSnapshot(
  overview: SummaryQuery,
  recent: Partial<RecentQuery>,
): Promise<DashboardSnapshot> {
  const resolved: RecentQuery = {
    limit: recent.limit ?? DEFAULT_RECENT_LIMIT,
    filter: recent.filter ?? {},
  };
  return call<DashboardSnapshot>(COMMANDS.dashboardSnapshot, {
    overview,
    recent: resolved,
  });
}

/**
 * The allowance half alone, at one revision.
 *
 * Same contract as {@link fetchDashboardSnapshot} — one read, one revision,
 * nothing that can interleave — but it computes only what an allowance row
 * draws. Callers that render no totals should use this: see
 * {@link RiskSnapshot} for what the difference costs.
 */
export function fetchRiskSnapshot(): Promise<RiskSnapshot> {
  return call<RiskSnapshot>(COMMANDS.quotaSnapshot);
}

/** Totals and breakdowns for one filter. */
export function fetchUsageSummary(query?: SummaryQuery): Promise<UsageSummary> {
  return call<UsageSummary>(COMMANDS.usageSummary, { query: query ?? null });
}

/** The most recent records, newest first. */
export function fetchRecentUsage(query?: Partial<RecentQuery>): Promise<UsageRecord[]> {
  const resolved: RecentQuery = {
    limit: query?.limit ?? DEFAULT_RECENT_LIMIT,
    filter: query?.filter ?? {},
  };
  return call<UsageRecord[]>(COMMANDS.recentUsage, { query: resolved });
}

/** Distinguishes an empty store from a filter that matched nothing. */
export function fetchUsageRecordCount(): Promise<number> {
  return call<number>(COMMANDS.usageRecordCount);
}

/** Static facts about the supported sources. Touches no source. */
export function fetchSourceCapabilities(): Promise<AdapterInfo[]> {
  return call<AdapterInfo[]>(COMMANDS.sourceCapabilities);
}

/** The freshest committed allowance snapshot per source and window. */
export function fetchUsageQuota(): Promise<UsageQuota[]> {
  return call<UsageQuota[]>(COMMANDS.usageQuota);
}

/**
 * Ask the backend to catch up now.
 *
 * Recovery and diagnostics. It submits one trigger and returns immediately;
 * the result arrives as a `data-changed` event like every other update.
 */
export function syncNow(): Promise<void> {
  return call<void>(COMMANDS.syncNow);
}

/** Tell the backend a window came forward, so it can submit a catch-up. */
export function reportWindowFocused(): Promise<void> {
  return call<void>(COMMANDS.windowFocused);
}

/** Whether any source is being synchronized right now. */
export function fetchIsSyncing(): Promise<boolean> {
  return call<boolean>(COMMANDS.isSyncing);
}

const ERROR_CODES: readonly ErrorCode[] = ["storage", "adapter", "invalid_request"];

/**
 * Every rejection reaching React is an `AppError`.
 *
 * Rust commands reject with one already; anything else — a missing command, a
 * transport failure — is wrapped so callers never have to inspect `unknown`.
 */
export function toAppError(error: unknown): AppError {
  if (typeof error === "object" && error !== null) {
    const candidate = error as Partial<AppError>;
    if (
      typeof candidate.message === "string" &&
      ERROR_CODES.includes(candidate.code as ErrorCode)
    ) {
      return { code: candidate.code as ErrorCode, message: candidate.message };
    }
  }

  const message = error instanceof Error ? error.message : String(error);
  return { code: "storage", message: message || "The usage data could not be loaded." };
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw toAppError(error);
  }
}
