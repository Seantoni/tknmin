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
const MINI_WIDTH = 268;

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

/** Make the compact window exactly as tall as what it is showing. */
export async function fitMiniWindowHeight(height: number): Promise<void> {
  await getCurrentWindow().setSize(new LogicalSize(MINI_WIDTH, height));
}

async function call(command: string): Promise<void> {
  try {
    await invoke<void>(command);
  } catch (error) {
    throw toAppError(error);
  }
}
