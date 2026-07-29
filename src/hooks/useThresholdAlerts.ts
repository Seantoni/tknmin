import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import { fetchActiveAlerts, fetchHandoffPrompt, snoozeAlert } from "../api/notifications";
import type { ThresholdAlert } from "../domain/notifications";

export function useThresholdAlerts() {
  const [alerts, setAlerts] = useState<ThresholdAlert[]>([]);

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
    let unlisten: (() => void) | undefined;

    void listen<ThresholdAlert[]>("threshold-alerts", (event) => {
      if (!cancelled) setAlerts(event.payload);
    }).then((fn) => {
      if (cancelled) fn();
      else unlisten = fn;
    });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, [reload]);

  const continueAlert = useCallback(async (dedupeKey: string) => {
    const next = await snoozeAlert(dedupeKey);
    setAlerts(next);
  }, []);

  const createHandoff = useCallback(async (_dedupeKey: string) => {
    const prompt = await fetchHandoffPrompt();
    try {
      await navigator.clipboard.writeText(prompt);
    } catch {
      // Clipboard can fail without focus; surface the prompt via the banner copy state.
    }
    return prompt;
  }, []);

  return { alerts, continueAlert, createHandoff, reload };
}
