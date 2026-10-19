// Capture session control. The capture service belongs to the world-engine
// extension; the agent reaches it through the plugin HTTP proxy
// (`/api/plugins/{plugin_id}/x/...`). A 404 there means the extension is not
// installed on this node.

import { ApiError, apiFetch } from "@/shared/api-fetch";

export type HealthLevel = "ok" | "warn" | "bad";
export type HealthId = "blur" | "texture" | "flicker" | "corridor" | "storage" | "clock";
export const HEALTH_IDS: readonly HealthId[] = ["blur", "texture", "flicker", "corridor", "storage", "clock"];

export interface CaptureExposure {
  locked: boolean;
  exposure_us: number;
  gain: number;
  mode: "walking" | "slow" | "short";
  applied?: boolean;
}

export interface CaptureState {
  state: "idle" | "recording" | "paused" | "closing";
  session_id: string | null;
  duration_s: number;
  site: string | null;
  floor: number | null;
  room: string | null;
  segment: number;
  frames: { total: number; dropped_pct: number } | null;
  health: { id: HealthId; level: HealthLevel; value: number }[];
  exposure: CaptureExposure | null;
  storage: { free_bytes: number; est_remaining_s: number } | null;
}

export const CAPTURE_STATE_PATH = "/api/plugins/com.altnautica.world-engine/x/atlas/capture/state";

export function startCapture(args: { room?: string; floor?: number }): Promise<{ ok: boolean; session_id: string }> {
  return apiFetch("/api/plugins/com.altnautica.world-engine/x/atlas/capture/start", {
    method: "POST",
    body: args,
  });
}

export function stopCapture(): Promise<CaptureState> {
  return apiFetch<CaptureState>("/api/plugins/com.altnautica.world-engine/x/atlas/capture/stop", {
    method: "POST",
  });
}

export function markCapture(args: { room: string; floor: number }): Promise<{ ok: boolean; segment: number }> {
  return apiFetch("/api/plugins/com.altnautica.world-engine/x/atlas/capture/mark", {
    method: "POST",
    body: args,
  });
}

export function putCaptureExposure(exposure: CaptureExposure): Promise<CaptureExposure> {
  return apiFetch<CaptureExposure>("/api/plugins/com.altnautica.world-engine/x/atlas/capture/exposure", {
    method: "PUT",
    body: { locked: exposure.locked, exposure_us: exposure.exposure_us, gain: exposure.gain, mode: exposure.mode },
  });
}

/** Whether a failed capture call means the extension is absent. */
export function isExtensionMissing(err: unknown): boolean {
  return err instanceof ApiError && err.status === 404;
}

/** The capture service's refusal code (`E_STORAGE_FULL`,
 *  `E_ALREADY_RECORDING`, `E_CLOCK_NOT_READY`, ...), else null. */
export function captureErrorCode(err: unknown): string | null {
  return err instanceof ApiError ? err.code : null;
}
