/**
 * Pace — will the allowance outlast the window? Mirrors
 * `src-tauri/src/domain/pace.rs`. Change these only alongside the Rust
 * definitions they mirror.
 *
 * Where the rest of the interface reports position ("7% left this week"),
 * pace answers the risk question: at the measured rate, does the allowance
 * run out before the window resets, and by how much. The headline value is a
 * length of time, not a percent.
 */

import type { SourceApp } from "./usage";

/**
 * The part a user acts on. Words and position carry it; `unknown` and
 * `notStarted` must never be rendered as `green`.
 */
export type PaceState =
  | "notStarted"
  | "green"
  | "amber"
  | "red"
  | "exhausted"
  | "unknown";

/** How a pace was measured, so the interface can qualify what it shows. */
export type PaceBasis =
  | { kind: "trailing"; minutes: number }
  | { kind: "sinceWindowOpen"; assumedAnchored: boolean };

/**
 * How the current burn compares to this source's own history at this local
 * hour. `noBaseline` when there is not enough history to say.
 */
export type PaceVsBaseline = "noBaseline" | "below" | "typical" | "above" | "farAbove";

/** One allowance window's pace and what it implies. */
export interface WindowPace {
  /** Identity, matching the quota's source / label / window so rows join. */
  sourceApp: SourceApp;
  label: string | null;
  windowMinutes: number;
  state: PaceState;
  /** Minutes of usage left at the measured pace. `null` when no pace could
   * be measured — which is not zero and must never render as zero. */
  runwayMinutes: number | null;
  /** When the allowance is projected to run out, as an absolute instant so
   * the interface counts down to it. `null` when it outlasts the window. */
  projectedExhaustionAt: string | null;
  /** Minutes earlier than reset it runs out. `null` unless `red`. */
  shortfallMinutes: number | null;
  /** Minutes of allowance to spare at reset. `null` unless it outlasts. */
  slackMinutes: number | null;
  /** Pace as an integer percentage of the affordable pace (240 = 2.4×). */
  paceRatioPercent: number | null;
  basis: PaceBasis | null;
  /** How the current burn compares to this source's own history this hour. */
  vsBaseline: PaceVsBaseline;
  /** The reading this rests on, so its age can be shown beside it. */
  observedAt: string;
}
