import { invoke } from "@tauri-apps/api/core";

import { toAppError } from "./usage";

export interface CursorConnectionStatus {
  connected: boolean;
}

export async function fetchCursorConnection(): Promise<CursorConnectionStatus> {
  return call("cursor_connection_status");
}

export async function connectCursorDashboard(
  sessionToken: string,
): Promise<CursorConnectionStatus> {
  return call("connect_cursor_dashboard", { sessionToken });
}

export async function disconnectCursorDashboard(): Promise<CursorConnectionStatus> {
  return call("disconnect_cursor_dashboard");
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw toAppError(error);
  }
}
