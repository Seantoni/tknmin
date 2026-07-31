/**
 * A shared clock that re-renders on a slow tick, for anything that shows a
 * relative age or counts down to an instant.
 *
 * Relative times are otherwise evaluated once at render and then frozen until
 * the next snapshot arrives — and the snapshot deliberately does not publish
 * during a quiet period, so "synced just now" would stay on screen for an
 * hour. A runway or a reset countdown is the same kind of number attached to
 * something a user acts on, so it ticks here.
 *
 * One minute matches the idle quota poll: ages only change coarseness at the
 * minute boundary after the first minute, and a faster tick would re-render
 * the dashboard for no visible change.
 */

import { useEffect, useState } from "react";

const TICK_MS = 60_000;

/** The current time, re-evaluated every `intervalMs`. */
export function useNow(intervalMs: number = TICK_MS): Date {
  const [now, setNow] = useState(() => new Date());

  useEffect(() => {
    const timer = setInterval(() => setNow(new Date()), intervalMs);
    return () => clearInterval(timer);
  }, [intervalMs]);

  return now;
}
