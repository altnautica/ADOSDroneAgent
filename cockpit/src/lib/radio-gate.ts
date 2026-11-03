// The client-side armed gate for writes that retune a radio or its region
// (link tuning, TX power, LoRa region). The agent's armed interlock refuses
// most of them on its own (409 `E_ARMED`); this gate keeps the confirm control
// disabled first, so the operator sees why before sliding. The arm state comes
// from the dashboard snapshot, which reports only what the heartbeat said.

import { ApiError, apiFetch, errorDetail } from "@/shared/api-fetch";
import type { DashboardSnapshot } from "@/shared/api/types";

export const DASHBOARD_SNAPSHOT_PATH = "/api/v1/dashboard/snapshot";

/** What the node knows about its vehicle's arm state. */
export interface ArmedReading {
  /** The flight controller link is up. */
  connected: boolean;
  /** The heartbeat's arm flag; null until the FC has reported it. */
  armed: boolean | null;
}

export async function getArmedReading(signal?: AbortSignal): Promise<ArmedReading> {
  const snap = await apiFetch<DashboardSnapshot>(DASHBOARD_SNAPSHOT_PATH, { signal });
  return {
    connected: snap?.fc?.connected === true,
    armed: typeof snap?.fc?.armed === "boolean" ? snap.fc.armed : null,
  };
}

/** Why a radio write is held back, or null when it may go. */
export type RadioWriteBlock = "armed" | "armedUnknown" | null;

/**
 * Armed refuses. A connected FC that has not yet reported its arm state, or a
 * snapshot the node could not read, proves nothing, so the write waits. A node
 * with no FC link carries no vehicle to protect and may write.
 */
export function radioWriteBlock(reading: ArmedReading | null | undefined): RadioWriteBlock {
  if (!reading) return "armedUnknown";
  if (reading.armed === true) return "armed";
  if (reading.connected && reading.armed === null) return "armedUnknown";
  return null;
}

/** Whether a failed write was the agent's armed refusal. */
export function isArmedRefusal(err: unknown): boolean {
  return err instanceof ApiError && err.status === 409 && err.code === "E_ARMED";
}

/** The agent's own wording of an armed refusal (`{"error":"E_ARMED",
 *  "message":"Vehicle is armed"}`), or null when `err` is not one. The arm
 *  state can change between the cockpit's last poll and the write, so the
 *  agent's refusal is shown as it said it. */
export function armedRefusalText(err: unknown): string | null {
  if (!(err instanceof ApiError) || !isArmedRefusal(err)) return null;
  const body = err.body;
  if (body && typeof body === "object" && "message" in body && typeof body.message === "string" && body.message) {
    return body.message;
  }
  return errorDetail(body, err.message);
}
