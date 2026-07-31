/**
 * Window commands — the only path from React to Rust for anything about the
 * windows themselves.
 *
 * Which window is on screen is decided in Rust rather than here, so the menu
 * bar item and the interface end up in the same state. Height is the exception:
 * only the rendered document knows how tall the compact window has to be.
 */

import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow, LogicalSize } from "@tauri-apps/api/window";

import { toAppError } from "./usage";

const COMMANDS = {
  showMini: "show_mini_window",
  showDashboard: "show_dashboard_window",
} as const;

/** Kept in step with `MINI_WIDTH` in `src-tauri/src/mini.rs`. */
export const MINI_WIDTH = 268;

/** One row per allowance needs the width the stacked panel cannot spare. */
export const MINI_WIDE_WIDTH = 600;

/** One block's width in the grid. Kept in step with `.q-cell` in `App.css`. */
export const MINI_GRID_CELL = 240;

/** The gap between blocks — and between the outermost ones and the panel's
 *  edge, which the panel's own padding already is. Kept in step with
 *  `.q-list.is-grid` and `.mini` in `App.css`. */
export const MINI_GRID_GAP = 12;

/** The grid is a single row of blocks: one cell each, the gaps between them,
 *  and the panel's horizontal padding on both ends. */
export function miniGridWidth(cells: number): number {
  return Math.max(1, cells) * MINI_GRID_CELL + Math.max(0, cells - 1) * MINI_GRID_GAP + 20;
}

/** Which interface to mount: the dashboard, or the compact window. */
export function currentWindowLabel(): string {
  return getCurrentWindow().label;
}

export function showMiniWindow(): Promise<void> {
  return call(COMMANDS.showMini);
}

export function showDashboardWindow(): Promise<void> {
  return call(COMMANDS.showDashboard);
}

/** Make the compact window exactly as tall as what it is showing, and as
 *  wide as the layout asks for: the content decides the height, the layout
 *  decides the width. */
export async function fitMiniWindowSize(width: number, height: number): Promise<void> {
  await getCurrentWindow().setSize(new LogicalSize(width, height));
}

async function call(command: string): Promise<void> {
  try {
    await invoke<void>(command);
  } catch (error) {
    throw toAppError(error);
  }
}
