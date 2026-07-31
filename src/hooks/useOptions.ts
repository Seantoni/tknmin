import { useCallback, useEffect, useState } from "react";
import { listen } from "@tauri-apps/api/event";

import { fetchOptions, saveOptions, OPTIONS_CHANGED } from "../api/options";
import type { AppOptions } from "../domain/options";
import { defaultOptions } from "../domain/options";
import type { AppError } from "../domain/usage";
import { toAppError } from "../api/usage";

export type OptionsStatus = "loading" | "ready" | "error";

export function useOptions() {
  const [status, setStatus] = useState<OptionsStatus>("loading");
  const [options, setOptions] = useState<AppOptions>(defaultOptions);
  const [error, setError] = useState<AppError | null>(null);
  const [isSaving, setIsSaving] = useState(false);

  const reload = useCallback(async () => {
    setStatus("loading");
    setError(null);
    try {
      const next = await fetchOptions();
      setOptions(next);
      setStatus("ready");
    } catch (caught) {
      setError(toAppError(caught));
      setStatus("error");
    }
  }, []);

  useEffect(() => {
    void reload();
  }, [reload]);

  // The compact window saves options too; without this its save would arrive
  // here only after this window had already written its older copy back.
  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;

    void listen<AppOptions>(OPTIONS_CHANGED, (event) => {
      if (cancelled) return;
      setOptions(event.payload);
      setStatus("ready");
    }).then((off) => {
      if (cancelled) off();
      else unlisten = off;
    });

    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const update = useCallback(async (next: AppOptions) => {
    setOptions(next);
    setIsSaving(true);
    setError(null);
    try {
      const saved = await saveOptions(next);
      setOptions(saved);
      setStatus("ready");
      return saved;
    } catch (caught) {
      const appError = toAppError(caught);
      setError(appError);
      throw appError;
    } finally {
      setIsSaving(false);
    }
  }, []);

  return { status, options, error, isSaving, reload, update };
}
