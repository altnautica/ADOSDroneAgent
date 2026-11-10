// The cockpit's alerts: the shared base rules (link, battery, GNSS, frozen
// video, uplink; `@/shared/hud/status-pill`) plus the cockpit's own RF link
// verdict, each with its icon.

import type { LucideIcon } from "lucide-react";
import { BatteryLow, BatteryWarning, Satellite, SignalZero, VideoOff, WifiOff } from "lucide-react";

import { linkDiagView } from "@/lib/link-diag";
import type { PreflightCheck } from "@/shared/hud/preflight";
import type { BatteryHealth, GsStatus, VehicleState } from "@/lib/types";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { baseAlerts, controllerLost, pilotFailsafe, sortAlerts, type BaseAlert, type BaseAlertInputs } from "@/shared/hud/status-pill";

export interface CockpitAlert extends BaseAlert {
  icon: LucideIcon;
}

export interface AlertInputs extends BaseAlertInputs {
  status: GsStatus | null;
  telemetry: VehicleState | null;
  battery: BatteryHealth | null;
}

const ICONS: Readonly<Record<string, LucideIcon>> = {
  link_lost: SignalZero,
  link_stale: SignalZero,
  batt_crit: BatteryWarning,
  batt_low: BatteryLow,
  gps_no_3d: Satellite,
  video_frozen: VideoOff,
  uplink: WifiOff,
};

export function computeAlerts(i: AlertInputs): CockpitAlert[] {
  const out: CockpitAlert[] = baseAlerts(i).map((a) => ({ ...a, icon: ICONS[a.id] ?? BatteryWarning }));
  const rf = linkDiagView(i.status?.link?.link_diag);
  if (rf?.actionable) {
    out.push({ id: "rf", level: "warning", title: rf.title, detail: rf.hint, icon: rf.icon });
  }
  return sortAlerts(out);
}

/** Base alerts that already report a pre-flight check's failure, so the
 *  pill never lists the same fault twice. */
const CHECK_COVERED_BY: Readonly<Record<string, readonly string[]>> = {
  link: ["link_lost", "link_stale"],
  gps: ["gps_no_3d"],
  battery: ["batt_low", "batt_crit"],
};

/** The pill inputs the cockpit feeds the shared status-pill rules beyond its
 *  alerts: the pilot frame's age (null with no pilot state), whether the
 *  agent answered its last status poll, a lost controller, and on the ground
 *  the pre-flight checklist's failing items. */
export function pillWiring(i: {
  pilot: { at: number; data: PilotState } | null;
  nowMs: number;
  /** The last status poll failed. */
  statusStale: boolean;
  armed: boolean | undefined;
  checks: readonly PreflightCheck[];
  alerts: readonly BaseAlert[];
}): {
  pilotAgeMs: number | null;
  agentReachable: boolean;
  controllerLost: boolean;
  preflightFailing: { id: string; label: string }[];
  fcFailsafe: boolean;
  failsafeCause: string | null;
} {
  const active = new Set(i.alerts.map((a) => a.id));
  // Unknown arm state (FC link down) counts as possibly airborne.
  const onGround = i.armed === false;
  return {
    pilotAgeMs: i.pilot ? Math.max(0, i.nowMs - i.pilot.at) : null,
    agentReachable: !i.statusStale,
    controllerLost: controllerLost(i.pilot?.data ?? null),
    ...pilotFailsafe(i.pilot?.data ?? null),
    preflightFailing: onGround
      ? i.checks.filter((c) => !c.ok && !(CHECK_COVERED_BY[c.id] ?? []).some((id) => active.has(id))).map((c) => ({ id: c.id, label: c.label }))
      : [],
  };
}
