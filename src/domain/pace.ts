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

/**
 * A confirmed allowance reading carried forward to the present. Mirrors
 * `src-tauri/src/domain/projection.rs`.
 *
 * Sources publish their percentages on their own schedule — Claude Code
 * refreshes its cache every several minutes while active and not at all while
 * idle — but token records are current to seconds. The backend fits the rate
 * between the two from the source's own history and carries the last confirmed
 * reading forward. Both numbers travel together so the interface can always
 * show what was measured beside what was inferred.
 *
 * A window is absent from this list whenever the projection was not earned:
 * too little history, a rate too scattered to trust, nothing spent since the
 * reading, or a reading too old to project from. Then the confirmed number
 * stands alone, which is what the interface did before this existed.
 */
export interface QuotaProjection {
  /** Identity, matching the quota's source / label / window so rows join. */
  sourceApp: SourceApp;
  label: string | null;
  windowMinutes: number;
  /** What the source last actually reported. */
  confirmedPercentTenths: number;
  /** When it reported it. */
  confirmedAt: string;
  /** Confirmed plus the spend since — what the window is believed to be now. */
  projectedPercentTenths: number;
  /** The derived part alone. */
  addedPercentTenths: number;
  /** The instant projected to. */
  projectedAt: string;
  /** Minutes of open-loop projection. Small is good. */
  spanMinutes: number;
  /** How many confirmed-reading pairs the fitted rate rests on. */
  pairs: number;
  /** How far a typical reading sits from the fitted rate, as a percent of it —
   * the median deviation, so it describes the fit rather than its worst
   * member and does not drift as readings accumulate. Meaningless, and
   * reported as zero, when `provisional`. */
  residualPercent: number;
  /** True when the rate rests on a single span with nothing to check it
   * against — measured, but not yet corroborated. */
  provisional: boolean;
}

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
  /** Whether the reading behind this is older than the window would normally
   * tolerate. Still measured from — the records show nothing was spent since,
   * so an old reading is an exact one — but said out loud, because a stale
   * reading can only ever be optimistic. */
  fromAgedReading: boolean;
}
