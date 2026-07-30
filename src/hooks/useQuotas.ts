/**
 * Live allowance windows and their freshness, and nothing else.
 *
 * The compact window shows only quotas, so it loads only quotas — but through
 * exactly the same contract the dashboard uses: subscribe to `data-changed`,
 * then fetch. Because both windows converge on the same committed revisions
 * they cannot drift apart, and the compact window costs the backend nothing
 * extra.
 *
 * Health travels with the quotas because a percentage without an age is a
 * claim the data cannot support: an allowance reading from an hour ago and one
 * from ten seconds ago look identical on screen otherwise.
 */

import { useCallback, useEffect, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import { DATA_CHANGED, fetchDashboardSnapshot, toAppError } from "../api/usage";
import type { AppError, DataChanged, SourceSyncHealth, UsageQuota } from "../domain/usage";
import type { QuotaProjection, WindowPace } from "../domain/pace";

export type QuotasStatus = "loading" | "ready" | "error";

export interface Quotas {
  status: QuotasStatus;
  quotas: UsageQuota[];
  pace: WindowPace[];
  /** Confirmed readings carried forward, for the windows that earned one. */
  projections: QuotaProjection[];
  health: SourceSyncHealth[];
  error: AppError | null;
}

export function useQuotas(): Quotas {
  const [status, setStatus] = useState<QuotasStatus>("loading");
  const [quotas, setQuotas] = useState<UsageQuota[]>([]);
  const [pace, setPace] = useState<WindowPace[]>([]);
  const [projections, setProjections] = useState<QuotaProjection[]>([]);
  const [health, setHealth] = useState<SourceSyncHealth[]>([]);
  const [error, setError] = useState<AppError | null>(null);

  const revision = useRef(-1);
  const isMounted = useRef(true);

  const load = useCallback(async () => {
    try {
      // The compact window renders no records, so it asks for none. The
      // snapshot still reads everything at one revision, which is what keeps
      // the two windows in agreement.
      const snapshot = await fetchDashboardSnapshot({}, { limit: 0 });
      if (!isMounted.current) return;
      revision.current = snapshot.revision;
      setQuotas(snapshot.quotas);
      setPace(snapshot.pace);
      setProjections(snapshot.projections);
      setHealth(snapshot.health);
      setStatus("ready");
      setError(null);
    } catch (caught) {
      if (!isMounted.current) return;
      setError(toAppError(caught));
      // Allowances already on screen stay there: they are the last thing the
      // sources actually said, and their age is shown beside them.
      if (quotas.length === 0) setStatus("error");
    }
  }, [quotas.length]);

  const loadRef = useRef(load);
  loadRef.current = load;

  useEffect(() => {
    isMounted.current = true;
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    void listen<DataChanged>(DATA_CHANGED, (event) => {
      if (cancelled) return;
      if (event.payload.revision <= revision.current) return;
      revision.current = event.payload.revision;
      void loadRef.current();
    }).then((off) => {
      if (cancelled) off();
      else unlisten = off;
    });

    // After subscribing, never before: an event landing during this fetch is
    // already listened for, and carries a higher revision than it returns.
    void loadRef.current();

    return () => {
      cancelled = true;
      isMounted.current = false;
      unlisten?.();
    };
  }, []);

  return { status, quotas, pace, projections, health, error };
}
