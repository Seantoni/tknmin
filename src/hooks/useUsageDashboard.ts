/**
 * Loads everything the dashboard shows.
 *
 * All of it comes from Tauri commands, so the interface has no data source
 * other than Rust — including the filtering, which the repository applies.
 *
 * Two summaries are fetched: `overview` always covers every record, so the
 * source breakdown stays stable and comparable while a filter is active, and
 * `summary` reflects the current selection.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import {
  fetchRecentUsage,
  fetchUsageQuota,
  fetchUsageRecordCount,
  fetchUsageSources,
  fetchUsageSummary,
  refreshLogs,
  toAppError,
} from "../api/usage";
import type {
  AdapterInfo,
  AppError,
  SourceApp,
  UsageQuota,
  UsageRecord,
  UsageSummary,
} from "../domain/usage";

export interface DashboardData {
  /** Unfiltered, for proportions and the total record count. */
  overview: UsageSummary;
  /** Reflects the active filter. */
  summary: UsageSummary;
  recent: UsageRecord[];
  sources: AdapterInfo[];
  recordCount: number;
  quotas: UsageQuota[];
}

export type DashboardStatus = "loading" | "ready" | "error";

/** The dashboard reports on a rolling window, never all-time grand totals. */
export const DASHBOARD_WINDOW_DAYS = 30;

export interface Dashboard {
  status: DashboardStatus;
  data: DashboardData | null;
  error: AppError | null;
  /** True while a reload runs with earlier data still on screen. */
  isRefreshing: boolean;
  /** null means every source. */
  selectedSource: SourceApp | null;
  /** Selecting the active source clears the filter. */
  toggleSource: (source: SourceApp) => void;
  clearFilter: () => void;
  reload: () => void;
  /** Rescan the logs on disk, then reload. This is the manual import. */
  refresh: () => void;
}

export function useUsageDashboard(): Dashboard {
  const [data, setData] = useState<DashboardData | null>(null);
  const [error, setError] = useState<AppError | null>(null);
  const [status, setStatus] = useState<DashboardStatus>("loading");
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [selectedSource, setSelectedSource] = useState<SourceApp | null>(null);

  // Guards against a slow response overwriting a newer one, and against
  // setting state after the component is gone.
  const requestId = useRef(0);
  const isMounted = useRef(true);
  const hasLoaded = useRef(false);

  const load = useCallback(async (source: SourceApp | null) => {
    const currentRequest = ++requestId.current;
    if (hasLoaded.current) {
      setIsRefreshing(true);
    } else {
      setStatus("loading");
    }

    // Everything below is scoped to the rolling window; only the all-time
    // record count is not, so an empty window can be told apart from an
    // empty store.
    const from = new Date(
      Date.now() - DASHBOARD_WINDOW_DAYS * 24 * 60 * 60 * 1000,
    ).toISOString();
    const filter = source === null ? { from } : { from, sources: [source] };

    try {
      const [overview, summary, recent, sources, recordCount, quotas] = await Promise.all([
        fetchUsageSummary({ from }),
        fetchUsageSummary(filter),
        fetchRecentUsage({ filter }),
        fetchUsageSources(),
        fetchUsageRecordCount(),
        fetchUsageQuota(),
      ]);
      if (!isMounted.current || currentRequest !== requestId.current) return;
      setData({ overview, summary, recent, sources, recordCount, quotas });
      setError(null);
      setStatus("ready");
      hasLoaded.current = true;
    } catch (caught) {
      if (!isMounted.current || currentRequest !== requestId.current) return;
      setError(toAppError(caught));
      setStatus("error");
    } finally {
      if (isMounted.current && currentRequest === requestId.current) {
        setIsRefreshing(false);
      }
    }
  }, []);

  useEffect(() => {
    isMounted.current = true;
    void load(selectedSource);
    return () => {
      isMounted.current = false;
    };
  }, [load, selectedSource]);

  const toggleSource = useCallback((source: SourceApp) => {
    setSelectedSource((current) => (current === source ? null : source));
  }, []);

  const clearFilter = useCallback(() => setSelectedSource(null), []);

  const reload = useCallback(() => {
    void load(selectedSource);
  }, [load, selectedSource]);

  const refresh = useCallback(() => {
    void (async () => {
      setIsRefreshing(true);
      try {
        await refreshLogs();
      } catch {
        // A failed scan still leaves earlier data on screen; reload below
        // surfaces a store-level error if there is one.
      }
      await load(selectedSource);
    })();
  }, [load, selectedSource]);

  // The startup import finishes after the window is already up; when it
  // lands, pull the freshly imported records onto the screen.
  const selectedSourceRef = useRef(selectedSource);
  selectedSourceRef.current = selectedSource;
  useEffect(() => {
    let unlisten: (() => void) | undefined;
    listen("usage-imported", () => {
      void load(selectedSourceRef.current);
    }).then((off) => {
      unlisten = off;
    });
    return () => unlisten?.();
  }, [load]);

  // Quota checks are cheap and run more frequently than full log imports.
  // Apply their event payload directly so the remaining allowance stays live
  // without re-fetching every dashboard aggregation once a minute.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    void listen<UsageQuota[]>("quotas-updated", (event) => {
      if (!cancelled) {
        setData((current) => (current === null ? null : { ...current, quotas: event.payload }));
      }
    }).then((off) => {
      if (cancelled) off();
      else unlisten = off;
    });
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  return {
    status,
    data,
    error,
    isRefreshing,
    selectedSource,
    toggleSource,
    clearFilter,
    reload,
    refresh,
  };
}
