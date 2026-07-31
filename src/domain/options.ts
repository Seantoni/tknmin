/**
 * User options — mirrors `src-tauri/src/domain/options.rs`.
 *
 * Thresholds decide when Tokens should warn that an allowance is running low.
 * One row per source so Claude, Cursor, and Codex can be tuned independently.
 */

import type { SourceApp } from "./usage";

/** What a per-source threshold measures. */
export type ThresholdMetric =
  | "remaining_percent"
  | "minutes_until_reset"
  | "projected_exhaustion";

export interface SourceThreshold {
  sourceApp: SourceApp;
  enabled: boolean;
  metric: ThresholdMetric;
  /** Percent 0–100, or minutes until reset, depending on `metric`. */
  value: number;
}

/** How the compact window lays its allowance rows out. */
export type MiniLayout = "stacked" | "horizontal";

export interface AppOptions {
  thresholds: SourceThreshold[];
  /** Whether the compact window floats above other applications. */
  miniAlwaysOnTop: boolean;
  /** Whether the compact window stacks its rows or puts each on one line. */
  miniLayout: MiniLayout;
}

export const THRESHOLD_METRICS: readonly ThresholdMetric[] = [
  "remaining_percent",
  "minutes_until_reset",
  "projected_exhaustion",
];

export const DEFAULT_THRESHOLD_VALUE: Record<ThresholdMetric, number> = {
  remaining_percent: 25,
  minutes_until_reset: 60,
  projected_exhaustion: 60,
};

export function defaultOptions(): AppOptions {
  const sources: SourceApp[] = ["cursor", "claude_code", "codex"];
  return {
    thresholds: sources.map((sourceApp) => ({
      sourceApp,
      enabled: true,
      metric: "remaining_percent",
      value: DEFAULT_THRESHOLD_VALUE.remaining_percent,
    })),
    miniAlwaysOnTop: true,
    miniLayout: "stacked",
  };
}

export function metricLabel(metric: ThresholdMetric): string {
  switch (metric) {
    case "remaining_percent":
      return "percent left";
    case "minutes_until_reset":
      return "minutes until reset";
    case "projected_exhaustion":
      return "runs out early";
  }
}

export function clampThresholdValue(metric: ThresholdMetric, value: number): number {
  const n = Number.isFinite(value) ? Math.round(value) : 0;
  if (metric === "remaining_percent") {
    return Math.min(100, Math.max(0, n));
  }
  return Math.min(10_080, Math.max(0, n));
}
