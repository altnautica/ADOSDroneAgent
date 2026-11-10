// The flight-controller modes a failsafe flies: return and landing, by the
// mode names the agent's state reports. ArduPilot names (QuadPlane included)
// and the PX4 names.

/** Return modes: ArduPilot `RTL`, `SMART_RTL`, `QRTL`, Copter `AUTO_RTL`
 *  (the battery failsafe's DO_LAND_START-or-RTL action), PX4 `AUTO.RTL`. */
export const FC_RETURN_MODES: readonly string[] = ["RTL", "SMART_RTL", "QRTL", "AUTO_RTL", "AUTO.RTL"];

/** Landing modes: ArduPilot `LAND`, `QLAND`, Plane `LOITER_ALT_QLAND`, PX4
 *  `AUTO.LAND` and `AUTO.PRECLAND`. */
export const FC_LAND_MODES: readonly string[] = ["LAND", "QLAND", "LOITER_ALT_QLAND", "AUTO.LAND", "AUTO.PRECLAND"];

/** Every return and landing mode, keyed by upper-case name. */
export const FC_FAILSAFE_MODES: Readonly<Record<string, true>> = Object.fromEntries(
  [...FC_RETURN_MODES, ...FC_LAND_MODES].map((m) => [m, true as const]),
);

/** Whether `mode` (any case) is a return or landing mode. */
export function isFcFailsafeMode(mode: string | null | undefined): boolean {
  return typeof mode === "string" && FC_FAILSAFE_MODES[mode.toUpperCase()] === true;
}
