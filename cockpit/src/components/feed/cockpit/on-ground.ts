// Whether the aircraft is on the ground, for the left column's Take off / Land:
// a fresh pilot's `landed` (or disarmed), else the telemetry's disarmed. An
// unknown arm state (the pilot not hearing the flight controller, or the FC
// link down) counts as possibly airborne. A stale pilot frame never decides
// it, so a frozen `landed` cannot swap Land for Take off in the air.

import { useFreshPilot } from "@/hooks/use-pilot";
import { useFlightStore } from "@/stores/flight-store";

interface GroundFrame {
  landed: boolean | null;
  armed: boolean | null;
}

/** From the fresh pilot frame (null when absent or stale). */
export function groundFrom(fresh: GroundFrame | null, telemetryArmed: boolean | null | undefined): boolean {
  return fresh ? fresh.landed === true || fresh.armed === false : telemetryArmed === false;
}

/** From the live stores: the fresh pilot (aged out twice a second) and the
 *  telemetry's arm state. */
export function useOnGround(): boolean {
  const fresh = useFreshPilot();
  const armed = useFlightStore((s) => s.telemetry?.armed);
  return groundFrom(fresh, armed);
}
