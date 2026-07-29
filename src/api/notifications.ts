/**
 * Alert commands — threshold notifications surface.
 */

import { invoke } from "@tauri-apps/api/core";

import type { ThresholdAlert } from "../domain/notifications";
import { toAppError } from "./usage";

const COMMANDS = {
  activeAlerts: "active_alerts",
  snoozeAlert: "snooze_alert",
  handoffPrompt: "handoff_prompt",
  testNotification: "test_notification",
} as const;

export function fetchActiveAlerts(): Promise<ThresholdAlert[]> {
  return call<ThresholdAlert[]>(COMMANDS.activeAlerts);
}

export function snoozeAlert(dedupeKey: string): Promise<ThresholdAlert[]> {
  return call<ThresholdAlert[]>(COMMANDS.snoozeAlert, { dedupeKey });
}

export function fetchHandoffPrompt(): Promise<string> {
  return call<string>(COMMANDS.handoffPrompt);
}

export function testNotification(): Promise<void> {
  return call<void>(COMMANDS.testNotification);
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(command, args);
  } catch (error) {
    throw toAppError(error);
  }
}
