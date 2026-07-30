import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import { fetchActiveAlerts, fetchHandoffPrompt, snoozeAlert } from "../api/notifications";
import type { ThresholdAlert } from "../domain/notifications";

export function useThresholdAlerts() {
  const [alerts, setAlerts] = useState<ThresholdAlert[]>([]);
  const [handoffCopiedKey, setHandoffCopiedKey] = useState<string | null>(null);

  const reload = useCallback(async () => {
    try {
      setAlerts(await fetchActiveAlerts());
    } catch {
      // Quotas may not be ready yet; stay quiet until the next event.
    }
  }, []);

  useEffect(() => {
    void reload();
    let cancelled = false;
    const unlisteners: Array<() => void> = [];

    void listen<ThresholdAlert[]>("threshold-alerts", (event) => {
      if (!cancelled) setAlerts(event.payload);
    }).then((fn) => {
      if (cancelled) fn();
      else unlisteners.push(fn);
    });

    void listen<string | null>("threshold-handoff-copied", (event) => {
      if (!cancelled && event.payload !== null) setHandoffCopiedKey(event.payload);
    }).then((fn) => {
      if (cancelled) fn();
      else unlisteners.push(fn);
    });

    return () => {
      cancelled = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, [reload]);

  const continueAlert = useCallback(async (dedupeKey: string) => {
    const next = await snoozeAlert(dedupeKey);
    setHandoffCopiedKey(null);
    setAlerts(next);
  }, []);

  const createHandoff = useCallback(async (dedupeKey: string) => {
    const prompt = await fetchHandoffPrompt();
    await navigator.clipboard.writeText(prompt);
    setHandoffCopiedKey(dedupeKey);
    return prompt;
  }, []);

  return { alerts, handoffCopiedKey, continueAlert, createHandoff, reload };
}
