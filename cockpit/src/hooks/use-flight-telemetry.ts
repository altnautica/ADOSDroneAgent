// The flight-telemetry poll: `GET /api/telemetry` for the whole shell while the
// page is visible, written into the flight store. It runs at 5 Hz while the Feed
// is open and at 1 Hz on every other screen, so the arm state that gates the
// setup offer, the controller tour and the draft guard is known everywhere.
//
// `live` is derived from attitude presence plus how long the vehicle stamp has
// stood still (measured on the client's monotonic clock, so agent/browser clock
// skew never blanks the instruments). The HUD draws a real horizon only when
// there is real attitude. A failed poll flips `stale`, drops `live` and keeps
// the last snapshot rather than blanking.

import { useEffect } from "react";

import { getTelemetry } from "@/lib/api";
import { pollIntervalMs, renderProfile } from "@/lib/render-profile";
import type { VehicleState } from "@/lib/types";
import { pollLoop } from "@/hooks/use-status-poll";
import { nextNoFixSince } from "@/shared/hud/minimap-mode";
import { INITIAL_FLIGHT_STATE, useFlightStore, type HomePoint } from "@/stores/flight-store";

export const FLIGHT_POLL_MS = 200;
/** The poll's pace with the Feed closed: enough for the arm state and the
 *  liveness verdict (well inside `LIVE_FRESH_MS`). */
export const OFF_FEED_POLL_MS = 1000;

/** How many mounted Feeds want the fast pace. */
let fastHolders = 0;

/** The poll's interval now: fast while a Feed is open, slow otherwise. */
export function flightPollMs(): number {
  return pollIntervalMs(fastHolders > 0 ? FLIGHT_POLL_MS : OFF_FEED_POLL_MS, renderProfile());
}

/** Held by the Feed while mounted: the shell's poll runs at the flight pace. */
export function useFlightTelemetryFast(): void {
  useEffect(() => {
    fastHolders += 1;
    return () => {
      fastHolders -= 1;
    };
  }, []);
}

/** How long the vehicle stamp may stand still before the reading stops
 *  counting as live. */
const LIVE_FRESH_MS = 4000;

/** Whether the snapshot carries usable attitude at all. */
export function hasUsableAttitude(t: VehicleState | null): boolean {
  const att = t?.attitude;
  return att != null && Number.isFinite(att.roll) && Number.isFinite(att.pitch);
}

/** The vehicle timestamp this snapshot carries, or null when it has none. */
export function vehicleStamp(t: VehicleState | null): string | null {
  return t?.last_update ?? t?.last_heartbeat ?? null;
}

/** Whether the snapshot carries usable, recent attitude. `msSinceStampMoved`
 *  is how long the stamp has stood unchanged (null when there is no stamp; the
 *  agent only emits vehicle fields it considers fresh, so that is trusted). */
export function isLive(t: VehicleState | null, msSinceStampMoved: number | null): boolean {
  if (!hasUsableAttitude(t)) return false;
  if (msSinceStampMoved == null) return true;
  return msSinceStampMoved < LIVE_FRESH_MS;
}

/** Whether the readings arrived over the radio rather than from a local FC. */
export function isRelayed(t: VehicleState | null): boolean {
  return t?.telemetry_source === "relayed";
}

/** The home point after this sample: captured at the disarmed→armed edge
 *  (where the autopilot sets home) when the sample has a position fix, kept
 *  otherwise. */
export function nextHome(
  prevHome: HomePoint | null,
  wasArmed: boolean,
  t: VehicleState | null,
): HomePoint | null {
  const armed = t?.armed === true;
  const lat = t?.position?.lat;
  const lon = t?.position?.lon;
  if (armed && !wasArmed && Number.isFinite(lat) && Number.isFinite(lon) && !(lat === 0 && lon === 0)) {
    return { lat: lat as number, lon: lon as number };
  }
  return prevHome;
}

/**
 * When the current flight started, for the band's flight timer. Set only on an
 * observed disarmed→armed edge: a vehicle already armed when the cockpit
 * opened has an unknown start, so the timer shows a dash rather than the time
 * since page load. `prevArmed` is the arm state at the last live sample, null
 * until one has been seen.
 */
export function nextArmedSince(
  prevArmed: boolean | null,
  prevSince: number | null,
  live: boolean,
  armed: boolean,
  nowMs: number,
): number | null {
  if (!live) return prevSince;
  if (!armed) return null;
  return prevArmed === false ? nowMs : prevSince;
}

/** Start the poll; returns stop, which clears the live verdict (home and the
 *  arm history stay: they belong to the flight, not to the poll). */
export function startFlightTelemetryPoll(): () => void {
  let lastStamp: string | null = null;
  let lastStampMovedAt = 0;
  let wasArmed = false;

  const stop = pollLoop(
    async (signal) => {
      try {
        const telemetry = await getTelemetry(signal);
        const stamp = vehicleStamp(telemetry);
        const nowMs = performance.now();
        if (stamp !== lastStamp) {
          lastStamp = stamp;
          lastStampMovedAt = nowMs;
        }
        const msSinceStampMoved = stamp == null ? null : nowMs - lastStampMovedAt;
        const prev = useFlightStore.getState();
        const home = nextHome(prev.home, wasArmed, telemetry);
        wasArmed = telemetry.armed === true;
        const live = isLive(telemetry, msSinceStampMoved);
        useFlightStore.setState({
          telemetry,
          stale: false,
          live,
          relayed: isRelayed(telemetry),
          home,
          lastLiveAt: live ? nowMs : prev.lastLiveAt,
          armedAtLastLive: live ? telemetry.armed === true : prev.armedAtLastLive,
          armedSince: nextArmedSince(
            prev.armedAtLastLive,
            prev.armedSince,
            live,
            telemetry.armed === true,
            Date.now(),
          ),
          // Kept with the telemetry, not per map: a minimap mounted later
          // (a view switch) keeps the frame it had.
          noFixSince: nextNoFixSince(prev.noFixSince, live ? (telemetry.gps?.fix_type ?? null) : null, Date.now()),
        });
      } catch {
        if (signal.aborted) return;
        // Provenance survives a failed poll: the last snapshot is still on
        // screen and its origin has not changed.
        useFlightStore.setState({ stale: true, live: false, noFixSince: null });
      }
    },
    flightPollMs,
  );

  return () => {
    stop();
    useFlightStore.setState((s) => ({ ...INITIAL_FLIGHT_STATE, home: s.home, armedAtLastLive: s.armedAtLastLive, armedSince: s.armedSince }));
  };
}

/** The shell's one flight-telemetry poll, mounted for the whole session. */
export function useFlightTelemetryPoll(): void {
  useEffect(() => startFlightTelemetryPoll(), []);
}
