// The Sensors settings page's logic: the picked rig file (parsed before any
// upload, so only a JSON object can reach the guarded replace) and the clock
// source figures.

import { formatSigma, parseRigFile } from "@/shared/api/sensors";

export const SENSORS_POLL_MS = 5_000;
export const CLOCK_POLL_MS = 5_000;

export type RigPick =
  | { state: "empty" }
  | { state: "ready"; name: string; rig: Record<string, unknown> }
  | { state: "invalid"; name: string; reason: "parse" | "shape"; detail: string };

export const NO_RIG: RigPick = { state: "empty" };

/** Parse a picked file's text. */
export function pickRig(name: string, text: string): RigPick {
  const parsed = parseRigFile(text);
  return parsed.ok
    ? { state: "ready", name, rig: parsed.rig }
    : { state: "invalid", name, reason: parsed.reason, detail: parsed.detail };
}

/** The document the replace sends, or null: only a parsed object is sent. */
export function rigPayload(pick: RigPick): Record<string, unknown> | null {
  return pick.state === "ready" ? pick.rig : null;
}

/** A signed clock offset in ns as a readable figure. */
export function formatOffset(offsetNs: number): string {
  return `${offsetNs < 0 ? "-" : ""}${formatSigma(Math.abs(offsetNs))}`;
}
