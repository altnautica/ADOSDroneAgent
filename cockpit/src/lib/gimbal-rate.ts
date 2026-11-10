// Gimbal rate control from the cockpit's gimbal pad: `GET /api/v1/gimbal/rate`
// says whether the vehicle takes rates now (a MAVLink flight controller with a
// fresh gimbal attitude); `POST` sends pitch and yaw rates the agent holds for
// `hold_ms` before stopping the gimbal itself. The pad resends while held at
// half the hold and sends a zero rate on release. At most one request is in
// flight, and a 429 backs the sender off before it tries again.

import { ApiError, apiFetch } from "@/shared/api-fetch";

export interface GimbalRateAvailability {
  available: boolean;
  reason: string | null;
}

/** Deg/s at full pad deflection. */
export const GIMBAL_MAX_DPS = 60;
export const GIMBAL_HOLD_MS = 500;
export const GIMBAL_RESEND_MS = GIMBAL_HOLD_MS / 2 - 50;
/** At most one rate request per this many ms while the rate changes, so a
 *  drag on the pad stays far inside the agent's per-caller request budget. */
export const GIMBAL_SEND_INTERVAL_MS = 100;
/** The first wait after a 429, doubled per further 429 up to the max. */
export const GIMBAL_BACKOFF_MS = 1000;
export const GIMBAL_BACKOFF_MAX_MS = 4000;
/** The rate an arrow key sends while the pad has focus. */
export const GIMBAL_KEY_DPS = 30;

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

export interface GimbalRateSender {
  /** The rate wanted now; zero stops the gimbal with one request at once. */
  set(pitchDps: number, yawDps: number): void;
  /** Stop sending; a held rate is ended with a zero. */
  dispose(): void;
}

/** Paces gimbal rate requests: the first non-zero rate goes at once, changes
 *  after it are coalesced to one request per `GIMBAL_SEND_INTERVAL_MS`
 *  carrying the newest rate, an unchanged held rate is resent every
 *  `GIMBAL_RESEND_MS` before the agent's hold runs out, and the return to
 *  zero sends one zero rate immediately. A `send` that returns a promise
 *  holds the next request until it settles; a 429 answer holds requests for
 *  `GIMBAL_BACKOFF_MS`, doubling per repeat. A zero held back by either goes
 *  as soon as it may. */
export function createGimbalRateSender(send: (pitchDps: number, yawDps: number) => unknown): GimbalRateSender {
  let pitch = 0;
  let yaw = 0;
  let dirty = false;
  let lastSentAt = 0;
  let timer: ReturnType<typeof setInterval> | null = null;
  let inFlight = false;
  let blockedUntil = 0;
  let backoffMs = 0;
  // A zero rate waiting on an answer or a backoff, with no pacing timer left.
  const flushZero = () => {
    if (dirty && timer === null) emit();
  };
  const emit = () => {
    const now = Date.now();
    if (inFlight || now < blockedUntil) {
      dirty = true;
      return;
    }
    dirty = false;
    lastSentAt = now;
    const r = send(pitch, yaw);
    if (!(r instanceof Promise)) return;
    inFlight = true;
    r.then(
      () => {
        backoffMs = 0;
      },
      (e: unknown) => {
        if (!(e instanceof ApiError && e.status === 429)) return;
        backoffMs = backoffMs === 0 ? GIMBAL_BACKOFF_MS : Math.min(GIMBAL_BACKOFF_MAX_MS, backoffMs * 2);
        blockedUntil = Date.now() + backoffMs;
        setTimeout(flushZero, backoffMs);
      },
    ).finally(() => {
      inFlight = false;
      flushZero();
    });
  };
  const stop = () => {
    if (timer === null) return;
    clearInterval(timer);
    timer = null;
    pitch = 0;
    yaw = 0;
    emit();
  };
  return {
    set(p, y) {
      if (p === 0 && y === 0) {
        stop();
        return;
      }
      if (p === pitch && y === yaw) return;
      pitch = p;
      yaw = y;
      if (timer !== null) {
        dirty = true;
        return;
      }
      emit();
      timer = setInterval(() => {
        if (dirty || Date.now() - lastSentAt >= GIMBAL_RESEND_MS) emit();
      }, GIMBAL_SEND_INTERVAL_MS);
    },
    dispose: stop,
  };
}
