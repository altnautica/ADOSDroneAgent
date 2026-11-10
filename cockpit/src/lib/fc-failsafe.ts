// The flight controller's own failsafe, read from what it reports when the
// pilot's flag is not there to say so: a return or landing mode this cockpit
// did not command, or a HEARTBEAT `system_status` of CRITICAL or EMERGENCY.
//
// A mode this cockpit commanded counts as its own while the flight
// controller entered it within `COMMANDED_MODE_WINDOW_MS` of the command and
// has not left it since. Once the mode changes, a later return to it is the
// flight controller's own.

import { useFlightStore } from "@/stores/flight-store";

/** The flight-controller modes a failsafe flies: return and landing. */
export const FC_FAILSAFE_MODES: Readonly<Record<string, true>> = { RTL: true, SMART_RTL: true, LAND: true, QRTL: true, QLAND: true };

/** The recovery holds the Mode sheet keeps live whatever else is pending. */
export const FC_RECOVERY_MODES: Readonly<Record<string, true>> = { ...FC_FAILSAFE_MODES, BRAKE: true };

/** HEARTBEAT `system_status` (MAV_STATE) values a failsafe reports. */
const MAV_STATE_CRITICAL = 5;
const MAV_STATE_EMERGENCY = 6;

/** How long after a mode command the flight controller may take to enter it
 *  and still count as following this cockpit. */
export const COMMANDED_MODE_WINDOW_MS = 5000;

/** What the flight controller reports: its mode name and HEARTBEAT
 *  `system_status` (null when not reported). */
export interface FcView {
  mode: string | null;
  systemStatus: number | null;
}

interface Commanded {
  modes: ReadonlySet<string>;
  at: number;
  /** The flight controller was seen in one of `modes` since the command. */
  entered: boolean;
}

let commanded: Commanded | null = null;
let unsubscribe: (() => void) | null = null;

/** Track one observed flight-controller mode against the last command. An
 *  unreported mode (a telemetry gap) changes nothing. */
function observe(mode: string | null, now: number): void {
  if (!commanded || mode === null) return;
  const name = mode.toUpperCase();
  const inWindow = now - commanded.at <= COMMANDED_MODE_WINDOW_MS;
  if (commanded.modes.has(name)) {
    if (commanded.entered || inWindow) commanded.entered = true;
    else commanded = null;
  } else if (commanded.entered || !inWindow) {
    commanded = null;
  }
}

/** Record that this cockpit just commanded one of `modes`. */
export function noteCommandedModes(modes: readonly string[], now = Date.now()): void {
  commanded = { modes: new Set(modes.map((m) => m.toUpperCase())), at: now, entered: false };
  // Follow the mode as it changes, so leaving the mode is seen even when no
  // gate is read in between.
  unsubscribe ??= useFlightStore.subscribe((s, prev) => {
    const mode = s.telemetry?.mode ?? null;
    if (mode !== (prev.telemetry?.mode ?? null)) observe(mode, Date.now());
  });
}

/** Forget the last command (tests). */
export function forgetCommandedModes(): void {
  commanded = null;
}

/** Whether the flight controller is in its own failsafe by what it reports. */
export function fcSideFailsafe(fc: FcView | null | undefined, now = Date.now()): boolean {
  if (!fc) return false;
  const mode = fc.mode?.toUpperCase() ?? null;
  observe(mode, now);
  if (fc.systemStatus === MAV_STATE_CRITICAL || fc.systemStatus === MAV_STATE_EMERGENCY) return true;
  if (mode === null || !FC_FAILSAFE_MODES[mode]) return false;
  return !(commanded?.entered === true && commanded.modes.has(mode));
}
