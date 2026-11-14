// The battery failsafe readiness item's one-tap fix. ArduPilot ships the low
// and critical battery actions as warn-only; when the pilot reports either
// one doing nothing, an RTL that would climb without GNSS, or actions that
// differ from the ADOS profile, every surface offers "Fix in Aircraft setup"
// on that item while disarmed. The fix opens Aircraft setup and runs the
// ground wizard's ADOS profile step, which lists the failsafe profile and
// writes nothing until the operator applies it.
// Unset levels, no monitor, an unread state and an RTL above the altitude
// limit get no fix (the levels belong to the initial tune). Pure.

import { specText } from "./spec-text";

/** The pilot's readiness item id. */
export const BATTERY_FAILSAFE_ID = "battery_failsafe";

/** The setup step the fix runs. */
export const BATTERY_FAILSAFE_FIX_STEP = { wizard: "ground", step: "ados_profile" } as const;

/** The fix action's label. */
export const BATTERY_FAILSAFE_FIX_KEY = "readiness.battery_failsafe.fix";

const FIXABLE_KEYS = [
  "readiness.battery_failsafe.none",
  "readiness.battery_failsafe.none_confined",
  "readiness.battery_failsafe.low_none",
  "readiness.battery_failsafe.low_none_confined",
  "readiness.battery_failsafe.critical_none",
  "readiness.battery_failsafe.confined_rtl",
  "readiness.battery_failsafe.differs",
  "readiness.battery_failsafe.differs_confined",
  "readiness.battery_failsafe.differs_low",
] as const;

/** Whether the readiness item `{id, ok, msg}` offers the fix: the battery
 *  failsafe item, failing because an action does nothing, RTL climbs
 *  without GNSS or the actions differ from the profile, on a vehicle known to be disarmed (`armed` null while
 *  unknown offers nothing). */
export function batteryFailsafeFixable(
  item: { id: string; ok: boolean; msg: string },
  armed: boolean | null,
): boolean {
  if (item.id !== BATTERY_FAILSAFE_ID || item.ok || armed !== false) return false;
  return FIXABLE_KEYS.some((k) => specText(k) === item.msg);
}
