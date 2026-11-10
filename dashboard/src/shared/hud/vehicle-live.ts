// Whether vehicle telemetry is live, from the vehicle's own readings rather
// than the HTTP read: the agent answers `/api/telemetry` with the vehicle
// fields stripped while the flight controller link is down, so a read that
// succeeds says nothing about the vehicle. A sample is live while it carries
// usable attitude and its vehicle stamp moved within `LIVE_FRESH_MS`.
//
// `nextLiveness` folds each new sample into the time of the last live sample
// and the arm state then, which the status pill's link rules read
// (`msSinceLive`, `armedAtLastLive`).

/** How long the vehicle stamp may stand still before the reading stops
 *  counting as live (ms). */
export const LIVE_FRESH_MS = 4000;

export interface LiveSample {
  armed?: boolean | null;
  attitude?: { roll?: number | null; pitch?: number | null } | null;
  last_update?: string | null;
  last_heartbeat?: string | null;
}

/** Whether the sample carries usable attitude at all. */
export function hasUsableAttitude(t: LiveSample | null | undefined): boolean {
  const att = t?.attitude;
  return att != null && Number.isFinite(att.roll) && Number.isFinite(att.pitch);
}

/** The vehicle stamp the sample carries, or null without one. */
export function vehicleStamp(t: LiveSample | null | undefined): string | null {
  return t?.last_update ?? t?.last_heartbeat ?? null;
}

/** Whether the sample is live. `msSinceStampMoved` is how long its stamp has
 *  stood unchanged (null with no stamp: the agent only emits vehicle fields
 *  it considers fresh, so that is trusted). */
export function isLive(t: LiveSample | null | undefined, msSinceStampMoved: number | null): boolean {
  if (!hasUsableAttitude(t)) return false;
  if (msSinceStampMoved == null) return true;
  return msSinceStampMoved < LIVE_FRESH_MS;
}

export interface Liveness {
  /** When the last sample folded in arrived; 0 before any. */
  sampleAt: number;
  stamp: string | null;
  stampMovedAt: number;
  /** The last sample was live. */
  live: boolean;
  /** When the last live sample arrived; null before any. */
  lastLiveAt: number | null;
  /** The arm state at the last live sample; null before any. */
  armedAtLastLive: boolean | null;
}

export const INITIAL_LIVENESS: Liveness = {
  sampleAt: 0,
  stamp: null,
  stampMovedAt: 0,
  live: false,
  lastLiveAt: null,
  armedAtLastLive: null,
};

/** The liveness after a sample that arrived at `atMs`. */
export function nextLiveness(prev: Liveness, t: LiveSample | null | undefined, atMs: number): Liveness {
  const stamp = vehicleStamp(t);
  const stampMovedAt = stamp !== prev.stamp ? atMs : prev.stampMovedAt;
  const live = isLive(t, stamp == null ? null : atMs - stampMovedAt);
  return {
    sampleAt: atMs,
    stamp,
    stampMovedAt,
    live,
    lastLiveAt: live ? atMs : prev.lastLiveAt,
    armedAtLastLive: live ? t?.armed === true : prev.armedAtLastLive,
  };
}

/** Milliseconds since the vehicle was last live at `nowMs`: 0 while the last
 *  sample is live and the read that brought it is current (`readLive`), null
 *  when it was never live this session (nothing to have lost). */
export function msSinceLive(l: Liveness, nowMs: number, readLive: boolean): number | null {
  if (l.live && readLive) return 0;
  return l.lastLiveAt === null ? null : Math.max(0, nowMs - l.lastLiveAt);
}
