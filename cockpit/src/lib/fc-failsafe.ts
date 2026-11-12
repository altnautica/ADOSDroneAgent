// The flight controller's own failsafe, read from what it reports when the
// pilot's flag is not there to say so: a return or landing mode this cockpit
// did not command, or a HEARTBEAT `system_status` of CRITICAL or EMERGENCY.
//
// A mode this cockpit commanded counts as its own while the flight
// controller entered it within `COMMANDED_MODE_WINDOW_MS` of the command and
// has not left it since. Once the mode changes, a later return to it is the
// flight controller's own.

import { FC_FAILSAFE_MODES } from "@/shared/hud/fc-failsafe-modes";
import { useFlightStore } from "@/stores/flight-store";

export { FC_FAILSAFE_MODES, FC_LAND_MODES, FC_RETURN_MODES } from "@/shared/hud/fc-failsafe-modes";

/** The flight controller's recovery holds, the one set every surface reads:
 *  each return and landing mode, Brake, and the hold modes of every family
 *  (Copter Loiter, Alt Hold and PosHold; Plane Cruise, FBWA, QLoiter and
 *  QHover; PX4 Position and Altitude). The Mode sheet keeps them live
 *  whatever else is pending, and they take the short recovery hold. */
export const FC_RECOVERY_MODES: Readonly<Record<string, true>> = {
  ...FC_FAILSAFE_MODES,
  ...Object.fromEntries(
    ["BRAKE", "LOITER", "ALT_HOLD", "POSHOLD", "CRUISE", "FBWA", "QLOITER", "QHOVER", "POSITION", "ALTITUDE"].map((m) => [m, true as const]),
  ),
};

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
/** A return or landing this cockpit sent whose answer is not in yet. */
let provisional: { modes: ReadonlySet<string>; at: number } | null = null;
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

/** Record a return or landing this cockpit commanded at `at`, once the flight
 *  controller's answer is in: a refusal claims nothing; an accepted answer,
 *  or no answer, claims it only once the flight controller is seen in it
 *  within the window; and a mode the flight controller was already in when
 *  the command went out stays its own. */
export function claimCommandedModes(
  modes: readonly string[],
  modeAtCommand: string | null | undefined,
  ack: { observed?: boolean; accepted?: boolean } | null | undefined,
  at: number,
): void {
  dropProvisionalClaim(at);
  const before = modeAtCommand?.toUpperCase() ?? null;
  if (before !== null && modes.some((m) => m.toUpperCase() === before)) return;
  if (ack?.observed && !ack.accepted) return;
  noteCommandedModes(modes, at);
}

/** Record, as it goes out at `at`, a return or landing this cockpit is
 *  sending, until its answer settles the claim (`claimCommandedModes`) or the
 *  send fails outright (`dropProvisionalClaim`). Only the Pause gate reads it
 *  (`fcFailsafeForPause`), so a brake is not refused as fighting a failsafe
 *  while the flight controller already flies this cockpit's own command. A
 *  mode the flight controller was already in stays its own. */
export function claimProvisionally(modes: readonly string[], modeAtCommand: string | null | undefined, at: number): void {
  const before = modeAtCommand?.toUpperCase() ?? null;
  if (before !== null && modes.some((m) => m.toUpperCase() === before)) return;
  provisional = { modes: new Set(modes.map((m) => m.toUpperCase())), at };
}

/** Drop the provisional claim made at `at`, if it still stands. */
export function dropProvisionalClaim(at: number): void {
  if (provisional?.at === at) provisional = null;
}

/** The flight controller's mode right now (null when not reported). */
export function currentFcMode(): string | null {
  return useFlightStore.getState().telemetry?.mode ?? null;
}

/** Forget the last command (tests). */
export function forgetCommandedModes(): void {
  commanded = null;
  provisional = null;
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

/** The Pause gate's read of the flight controller's own failsafe: as
 *  `fcSideFailsafe`, except that a return or landing this cockpit sent within
 *  the window, whose answer is not in yet, is taken as its own. A critical
 *  heartbeat is a failsafe whatever was sent. */
export function fcFailsafeForPause(fc: FcView | null | undefined, now = Date.now()): boolean {
  if (!fcSideFailsafe(fc, now)) return false;
  if (fc!.systemStatus === MAV_STATE_CRITICAL || fc!.systemStatus === MAV_STATE_EMERGENCY) return true;
  const mode = fc!.mode?.toUpperCase() ?? null;
  const pending = provisional !== null && now - provisional.at <= COMMANDED_MODE_WINDOW_MS;
  return !(pending && mode !== null && provisional!.modes.has(mode));
}
