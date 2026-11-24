// The Vision panel's state rules, apart from its components: what the engine
// card shows, how a restart of the vision unit resolves, and a layer row's
// live status.

import { apiFetch, ApiError } from "@/shared/api-fetch";
import { HIDE_AFTER_MS } from "@/shared/hud/layer-engine";
import type { VisionEngineStatus } from "@/stores/layer-chips-store";

/** How long a start (or restart) of the vision unit may take. */
export const VISION_START_MS = 30_000;
/** How long the agent takes to come back and start vision after its restart. */
export const AGENT_RESTART_MS = 45_000;

export type EnginePending =
  | { kind: "starting" | "agent_restarting"; since: number }
  | { kind: "needs_agent_restart" }
  | null;

export type EngineCardState =
  | "running"
  | "starting"
  | "off"
  | "unreachable"
  | "unavailable"
  | "needs_agent_restart"
  | "agent_restarting"
  | "start_timeout"
  | "checking";

/** Whether a status read shows the engine running a usable backend. */
export function engineRunning(status: VisionEngineStatus | null, error: boolean): boolean {
  return status !== null && !error && status.backend_state !== "unavailable";
}

/** What the card shows, from the stored flag, the last status read and the
 *  action in flight. */
export function engineCardState(i: {
  enabled: boolean | null;
  status: VisionEngineStatus | null;
  error: boolean;
  pending: EnginePending;
  now: number;
}): EngineCardState {
  if (engineRunning(i.status, i.error)) return "running";
  const p = i.pending;
  if (p?.kind === "starting") return i.now - p.since < VISION_START_MS ? "starting" : "start_timeout";
  if (p?.kind === "agent_restarting") return i.now - p.since < AGENT_RESTART_MS ? "agent_restarting" : "start_timeout";
  if (p?.kind === "needs_agent_restart") return "needs_agent_restart";
  if (i.enabled === false) return "off";
  if (i.status !== null && !i.error) return "unavailable";
  if (i.error) return "unreachable";
  return "checking";
}

/** Restart the vision unit. Resolves "ok", "refused" (the agent does not
 *  restart that unit through its API) or throws the failure. */
export async function restartVisionUnit(): Promise<"ok" | "refused"> {
  try {
    const r = await apiFetch<{ status?: string; message?: string }>("/api/services/ados-vision/restart", { method: "POST", body: {} });
    if (r?.status === "ok") return "ok";
    if (typeof r?.message === "string" && r.message.startsWith("Unknown service")) return "refused";
    throw new Error(r?.message ?? "restart failed");
  } catch (e) {
    if (e instanceof ApiError && (e.status === 403 || e.status === 404)) return "refused";
    throw e;
  }
}

/** A layer's live state: frames arriving, none for {@link HIDE_AFTER_MS}, or
 *  the engine that makes them is off. */
export type LayerLiveState = "receiving" | "no_data" | "engine_off";

export function layerLiveState(seenAt: number, hasData: boolean, now: number, engineUp: boolean | null): LayerLiveState {
  if (engineUp === false) return "engine_off";
  return hasData && now - seenAt < HIDE_AFTER_MS ? "receiving" : "no_data";
}
