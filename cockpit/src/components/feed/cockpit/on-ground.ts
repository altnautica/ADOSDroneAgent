// Whether the aircraft is on the ground, for the left column's Take off / Land:
// a fresh pilot's `landed` (or disarmed), else the telemetry's disarmed. An
// unknown telemetry arm state (FC link down) counts as possibly airborne. A
// stale pilot frame never decides it, so a frozen `landed` cannot swap Land
// for Take off in the air.

import { pilotFresh } from "@/shared/hud/pilot-fresh";

interface GroundFrame {
  landed: boolean;
  armed: boolean;
}

/** From the fresh pilot frame (null when absent or stale). */
export function groundFrom(fresh: GroundFrame | null, telemetryArmed: boolean | null | undefined): boolean {
  return fresh ? fresh.landed || !fresh.armed : telemetryArmed === false;
}

/** From a stamped pilot frame, judged fresh at `nowMs`. */
export function onGround(pilot: { at: number; data: GroundFrame } | null, telemetryArmed: boolean | null | undefined, nowMs: number): boolean {
  return groundFrom(pilot && pilotFresh(pilot.at, nowMs) ? pilot.data : null, telemetryArmed);
}
