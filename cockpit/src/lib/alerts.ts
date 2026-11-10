// The cockpit's alerts: the shared base rules (link, battery, GNSS, frozen
// video, uplink; `@/shared/hud/status-pill`) plus the cockpit's own RF link
// verdict, each with its icon.

import type { LucideIcon } from "lucide-react";
import { BatteryLow, BatteryWarning, Satellite, SignalZero, VideoOff, WifiOff } from "lucide-react";

import { linkDiagView } from "@/lib/link-diag";
import type { BatteryHealth, GsStatus, VehicleState } from "@/lib/types";
import { baseAlerts, sortAlerts, type BaseAlert, type BaseAlertInputs } from "@/shared/hud/status-pill";

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
