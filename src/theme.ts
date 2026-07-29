/**
 * The few things that need a shared visual identity.
 *
 * One hue per source, desaturated enough to sit on a near-black background
 * without vibrating. Nothing else in the interface is colored, so these read
 * as data rather than decoration.
 */

import type { SourceApp } from "./domain/usage";

export const SOURCE_COLOR: Record<SourceApp, string> = {
  cursor: "#7aa2f7",
  claude_code: "#dda06a",
  codex: "#6cc4a1",
};

/** Lowercase on purpose: these are labels, not headings. */
export const SOURCE_LABEL: Record<SourceApp, string> = {
  cursor: "cursor",
  claude_code: "claude code",
  codex: "codex",
};
