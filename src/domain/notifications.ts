/**
 * Threshold alerts — mirrors `src-tauri/src/domain/notifications.rs`.
 */

import type { ThresholdMetric } from "./options";
import type { SourceApp } from "./usage";

export type AlertAction = "create_handoff" | "continue";

export interface ThresholdAlert {
  sourceApp: SourceApp;
  windowMinutes: number;
  metric: ThresholdMetric;
  thresholdValue: number;
  remainingPercentTenths: number;
  minutesUntilReset: number;
  resetsAt: string;
  observedAt: string;
  dedupeKey: string;
}
