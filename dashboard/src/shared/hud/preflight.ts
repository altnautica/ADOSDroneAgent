// The pre-flight checks a surface can evaluate from what the agent reports,
// and the ones the status pill lists: on the ground only, and never a check
// whose fault a base alert already reports (the pill never lists the same
// fault twice). EKF health is not on the telemetry wire, so it is not listed.

import { batteryLowPct, batteryReading, type BatteryHealthInput, type FcBatteryInput } from "./battery";

export interface PreflightCheck {
  id: string;
  label: string;
  ok: boolean;
}

export interface PreflightInputs {
  telemetry: (FcBatteryInput & { gps?: { fix_type?: number | null } | null }) | null;
  battery: BatteryHealthInput | null;
  /** Vehicle telemetry is live (see `vehicle-live`). */
  live: boolean;
  /** The home point, or null while none is known. */
  home: object | null;
}

/** The checks, in checklist order. */
export function preflightChecks(i: PreflightInputs): PreflightCheck[] {
  const fix = i.telemetry?.gps?.fix_type;
  const reading = batteryReading(i.battery, i.telemetry);
  const low = batteryLowPct(i.battery);
  return [
    { id: "link", label: "Vehicle link fresh", ok: i.live },
    { id: "gps", label: "GPS fix 3D or better", ok: i.live && typeof fix === "number" && fix >= 3 },
    { id: "battery", label: "Battery above the low band", ok: reading !== null && reading.pct > low },
    { id: "home", label: "Home position set", ok: i.home !== null },
  ];
}

/** Base alerts that already report a pre-flight check's failure. */
export const CHECK_COVERED_BY: Readonly<Record<string, readonly string[]>> = {
  link: ["link_lost", "link_stale"],
  gps: ["gps_no_3d"],
  battery: ["batt_low", "batt_crit"],
};

/** The failing checks the pill lists: none unless on the ground (not
 *  armed), and none a listed base alert already covers. */
export function preflightFailing(
  checks: readonly PreflightCheck[],
  alerts: readonly { id: string }[],
  armed: boolean | null | undefined,
): { id: string; label: string }[] {
  if (armed === true) return [];
  const active = new Set(alerts.map((a) => a.id));
  return checks
    .filter((c) => !c.ok && !(CHECK_COVERED_BY[c.id] ?? []).some((id) => active.has(id)))
    .map((c) => ({ id: c.id, label: c.label }));
}
