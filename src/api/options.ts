/**
 * Options commands — the only path from React to Rust for settings.
 */

import { invoke } from "@tauri-apps/api/core";

import type { AppOptions } from "../domain/options";
import { toAppError } from "./usage";

const COMMANDS = {
  getOptions: "get_options",
  setOptions: "set_options",
} as const;

export function fetchOptions(): Promise<AppOptions> {
  return call<AppOptions>(COMMANDS.getOptions);
}

export function saveOptions(options: AppOptions): Promise<AppOptions> {
  return call<AppOptions>(COMMANDS.setOptions, { options });
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw toAppError(error);
  }
}
