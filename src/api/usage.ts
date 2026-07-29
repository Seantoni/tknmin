/**
 * The only path from React to Rust.
 *
 * Components call these functions rather than `invoke` directly, so command
 * names, argument shapes, and error handling live in one place.
 */

import { invoke } from "@tauri-apps/api/core";

import type {
  AdapterInfo,
  AppError,
  ErrorCode,
  RecentQuery,
  RefreshReport,
  SummaryQuery,
  UsageQuota,
  UsageRecord,
  UsageSummary,
} from "../domain/usage";

const COMMANDS = {
  usageSummary: "usage_summary",
  recentUsage: "recent_usage",
  usageRecordCount: "usage_record_count",
  usageSources: "usage_sources",
  usageQuota: "usage_quota",
  refreshLogs: "refresh_logs",
} as const;

export const DEFAULT_RECENT_LIMIT = 50;

/** Totals and breakdowns for the dashboard. */
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

/** The source applications this build knows about. */
export function fetchUsageSources(): Promise<AdapterInfo[]> {
  return call<AdapterInfo[]>(COMMANDS.usageSources);
}

/** Rescan every source's logs on disk and import whatever is new. */
export function refreshLogs(): Promise<RefreshReport> {
  return call<RefreshReport>(COMMANDS.refreshLogs);
}

/** The freshest allowance-window snapshot per source, when reported. */
export function fetchUsageQuota(): Promise<UsageQuota[]> {
  return call<UsageQuota[]>(COMMANDS.usageQuota);
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
