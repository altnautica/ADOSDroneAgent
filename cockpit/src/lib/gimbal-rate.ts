// Gimbal rate control from the cockpit's gimbal pad: `GET /api/v1/gimbal/rate`
// says whether the vehicle takes rates now (a MAVLink flight controller with a
// fresh gimbal attitude); `POST` sends pitch and yaw rates the agent holds for
// `hold_ms` before stopping the gimbal itself. The pad resends while held at
// half the hold and sends a zero rate on release.

import { ApiError, apiFetch } from "@/shared/api-fetch";

export interface GimbalRateAvailability {
  available: boolean;
  reason: string | null;
}

/** Deg/s at full pad deflection. */
export const GIMBAL_MAX_DPS = 60;
export const GIMBAL_HOLD_MS = 500;
export const GIMBAL_RESEND_MS = GIMBAL_HOLD_MS / 2 - 50;

/** Whether the pad can show. An agent without the route (404) cannot take
 *  rates. */
export async function gimbalRateAvailability(signal?: AbortSignal): Promise<GimbalRateAvailability> {
  try {
    const body = await apiFetch<Partial<GimbalRateAvailability>>("/api/v1/gimbal/rate", { signal, skipAuthSignal: true });
    return { available: body.available === true, reason: body.reason ?? null };
  } catch (e) {
    if (e instanceof ApiError && e.status === 404) return { available: false, reason: null };
    throw e;
  }
}

export function sendGimbalRate(pitchDps: number, yawDps: number): Promise<unknown> {
  const clamp = (v: number) => Math.max(-GIMBAL_MAX_DPS, Math.min(GIMBAL_MAX_DPS, Number.isFinite(v) ? v : 0));
  return apiFetch("/api/v1/gimbal/rate", {
    method: "POST",
    body: { pitch_dps: clamp(pitchDps), yaw_dps: clamp(yawDps), hold_ms: GIMBAL_HOLD_MS },
  });
}
