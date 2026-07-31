/**
 * Loads everything the dashboard shows, at one revision.
 *
 * The interface owns no refresh cadence. There is no timer here, no scan
 * command, and no polling: Rust's coordinator decides when sources are read,
 * commits a revision, and says so. This hook's whole job is the subscription
 * handshake that makes that safe against races:
 *
 *   1. subscribe to `data-changed` **before** fetching, so an event emitted
 *      during the initial load cannot be lost
 *   2. fetch one snapshot, which names the revision it was read at
 *   3. ignore every event at or below that revision
 *   4. fetch again only on a higher one
 *
 * Doing it in that order is what closes the gap where a commit lands between
 * a fetch returning and a listener attaching.
 *
 * A revision carries one more bit — whether any number actually moved — and
 * the hook reads two different amounts of data depending on it. Every revision
 * refreshes the allowances, because those are what change fastest and cost
 * almost nothing to re-read; only a revision that moved data re-summarises the
 * store behind them, because only then can a total differ. See `refreshRisk`.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import {
  DATA_CHANGED,
  fetchDashboardSnapshot,
  fetchRiskSnapshot,
  fetchSourceCapabilities,
  syncNow,
  toAppError,
} from "../api/usage";
import type {
  AdapterInfo,
  AppError,
  DashboardSnapshot,
  DataChanged,
  SourceApp,
} from "../domain/usage";

export type DashboardStatus = "loading" | "ready" | "error";

/** The dashboard reports on a rolling window, never all-time grand totals. */
export const DASHBOARD_WINDOW_DAYS = 30;

export interface Dashboard {
  status: DashboardStatus;
  /** The last snapshot that loaded. Stays on screen while a newer one loads. */
  snapshot: DashboardSnapshot | null;
  /** Static per-source facts, fetched once. */
  sources: AdapterInfo[];
  error: AppError | null;
  /** True while a reload runs with earlier data still on screen. */
  isRefreshing: boolean;
  /** null means every source. */
  selectedSource: SourceApp | null;
  /** Selecting the active source clears the filter. */
  toggleSource: (source: SourceApp) => void;
  clearFilter: () => void;
  reload: () => void;
  /** Recovery only: asks the backend to catch up now. */
  requestSync: () => void;
}

export function useUsageDashboard(): Dashboard {
  const [snapshot, setSnapshot] = useState<DashboardSnapshot | null>(null);
  const [sources, setSources] = useState<AdapterInfo[]>([]);
  const [error, setError] = useState<AppError | null>(null);
  const [status, setStatus] = useState<DashboardStatus>("loading");
  const [isRefreshing, setIsRefreshing] = useState(false);
  const [selectedSource, setSelectedSource] = useState<SourceApp | null>(null);

  // Guards against a slow response overwriting a newer one, and against
  // setting state after the component is gone.
  const requestId = useRef(0);
  const isMounted = useRef(true);
  const hasLoaded = useRef(false);
  /** The highest revision already on screen, or already being fetched for. */
  const revision = useRef(-1);

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
      const loaded = await fetchDashboardSnapshot({ from }, { filter });
      // A response for a filter the user has since changed describes the
      // wrong question, however fresh it is.
      if (!isMounted.current || currentRequest !== requestId.current) return;
      revision.current = loaded.revision;
      setSnapshot(loaded);
      setError(null);
      setStatus("ready");
      hasLoaded.current = true;
    } catch (caught) {
      if (!isMounted.current || currentRequest !== requestId.current) return;
      setError(toAppError(caught));
      // A failed fetch with data already on screen keeps that data: those
      // numbers are last-known-good rather than wrong, and blanking them
      // would hide more than the message explains.
      if (!hasLoaded.current) setStatus("error");
    } finally {
      if (isMounted.current && currentRequest === requestId.current) {
        setIsRefreshing(false);
      }
    }
  }, []);

  /**
   * Refresh the allowance half alone, leaving the totals where they are.
   *
   * Sound because of exactly what `dataChanged: false` promises: no record and
   * no quota moved at that revision, only freshness did. Totals read at an
   * earlier revision are therefore not stale figures being tolerated — they
   * are still the same numbers the new revision would produce, so carrying
   * them forward states nothing untrue.
   *
   * What it buys is the whole point. A dashboard snapshot summarises every
   * record in the store; on a fifty-thousand-record store that is about 460ms,
   * against 30ms for this. Paying the former on every quota poll — once a
   * minute while idle, every twelve seconds while a source is in use — spent
   * most of a second re-deriving totals that could not have changed, and it
   * put that delay between a moving allowance and the screen.
   */
  const refreshRisk = useCallback(async () => {
    try {
      const risk = await fetchRiskSnapshot();
      if (!isMounted.current) return;
      setSnapshot((held) => {
        // Nothing to merge into yet, and a full load is already on its way.
        if (held === null) return held;
        // A full load that landed later describes a newer world; this reply
        // has nothing to add to it.
        if (risk.revision < held.revision) return held;
        return {
          ...held,
          revision: risk.revision,
          quotas: risk.quotas,
          health: risk.health,
          pace: risk.pace,
          projections: risk.projections,
        };
      });
    } catch {
      // The allowances already on screen are last-known-good, and the next
      // revision will try again. A failed background refresh is not worth an
      // error state over data the user can still read.
    }
  }, []);

  useEffect(() => {
    isMounted.current = true;
    return () => {
      isMounted.current = false;
    };
  }, []);

  const selectedRef = useRef(selectedSource);
  selectedRef.current = selectedSource;

  // The subscription is attached before the first fetch and torn down after
  // it, so no committed revision can slip through the gap around either.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    void listen<DataChanged>(DATA_CHANGED, (event) => {
      if (cancelled) return;
      // Already covered by what is on screen, or by a fetch in flight.
      if (event.payload.revision <= revision.current) return;
      revision.current = event.payload.revision;
      if (event.payload.dataChanged || !hasLoaded.current) {
        void load(selectedRef.current);
      } else {
        void refreshRisk();
      }
    }).then((off) => {
      if (cancelled) off();
      else unlisten = off;
    });

    void load(selectedRef.current);

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [load, refreshRisk]);

  // Changing the filter re-reads at whatever revision is current. The
  // revision guard is deliberately not reset: the data has not changed, only
  // the question being asked of it.
  const isFirstRender = useRef(true);
  useEffect(() => {
    if (isFirstRender.current) {
      isFirstRender.current = false;
      return;
    }
    void load(selectedSource);
  }, [load, selectedSource]);

  // Static capabilities never change while the app runs, so they are fetched
  // once rather than travelling in every snapshot.
  useEffect(() => {
    let cancelled = false;
    void fetchSourceCapabilities()
      .then((loaded) => {
        if (!cancelled) setSources(loaded);
      })
      .catch(() => {
        // Nothing on this screen depends on it; the footer simply says less.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const toggleSource = useCallback((source: SourceApp) => {
    setSelectedSource((current) => (current === source ? null : source));
  }, []);

  const clearFilter = useCallback(() => setSelectedSource(null), []);

  const reload = useCallback(() => {
    void load(selectedRef.current);
  }, [load]);

  const requestSync = useCallback(() => {
    // Fire and forget: the answer arrives as a committed revision, not as a
    // return value.
    void syncNow().catch(() => {});
  }, []);

  return {
    status,
    snapshot,
    sources,
    error,
    isRefreshing,
    selectedSource,
    toggleSource,
    clearFilter,
    reload,
    requestSync,
  };
}
