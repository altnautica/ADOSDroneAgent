// The cockpit's alert rules, as one pure function of what the panel knows.
// Three levels:
//   critical — vehicle link lost > 10 s while armed, battery at or below the
//              critical band, a critical battery-engine anomaly
//   warning  — vehicle link stale > 3 s, battery at or below the low band, a
//              GPS fix below 3D while armed, frozen video, an actionable RF
//              link verdict, a battery-engine warning
//   advisory — the ground station's uplink is offline
// Every rule reads a real field; an unknown reading never alerts. In
// particular an unknown battery (no engine reading, `-1`, null, or a pack
// voltage that says nobody is measuring) never raises a battery alert.

import type { LucideIcon } from "lucide-react";
import { BatteryLow, BatteryWarning, Satellite, SignalZero, VideoOff, WifiOff } from "lucide-react";

import { linkDiagView } from "@/lib/link-diag";
import type { BatteryHealth, GsStatus, VehicleState } from "@/lib/types";
import type { VideoFeedState } from "@/shared/video-transport";

export type AlertLevel = "critical" | "warning" | "advisory";

export interface CockpitAlert {
  id: string;
  level: AlertLevel;
  title: string;
  detail?: string;
  icon: LucideIcon;
}

export const LINK_STALE_MS = 3000;
export const LINK_LOST_MS = 10_000;
export const BATTERY_LOW_PCT = 20;
export const BATTERY_CRITICAL_PCT = 10;

/** Whether a pack voltage is a real measurement: MAVLink reports 65535 mV with
 *  no battery monitor and 0 with nothing attached (the agent divides by 1000,
 *  so the exact sentinel is reversed; a real 65.5 V pack is 65500 mV). */
export function batteryIsMeasured(voltage: number | null | undefined): boolean {
  if (typeof voltage !== "number" || !Number.isFinite(voltage)) return false;
  const mv = Math.round(voltage * 1000);
  return mv > 0 && mv !== 65535;
}

export interface BatteryReading {
  pct: number;
  /** Seconds until the reserve threshold, from the battery engine. */
  timeToReserveS: number | null;
  source: "engine" | "fc";
}

/**
 * The battery the cockpit trusts: the battery engine's freshest pack when the
 * node runs one, else the flight controller's own percentage corroborated by a
 * measured pack voltage. Null means unknown, which is shown as a dash and
 * never alerts.
 */
export function batteryReading(
  battery: BatteryHealth | null,
  telemetry: VehicleState | null,
): BatteryReading | null {
  if (battery && battery.enabled && !battery.stale) {
    const pack = battery.packs.find(
      (p) => !p.stale && typeof p.remaining_pct === "number" && p.remaining_pct >= 0,
    );
    if (pack) {
      const eta = pack.prediction?.eta_s;
      return {
        pct: pack.remaining_pct as number,
        timeToReserveS: typeof eta === "number" && Number.isFinite(eta) && eta >= 0 ? eta : null,
        source: "engine",
      };
    }
  }
  const remaining = telemetry?.battery?.remaining;
  if (
    typeof remaining === "number" &&
    Number.isFinite(remaining) &&
    remaining >= 0 &&
    batteryIsMeasured(telemetry?.battery?.voltage)
  ) {
    return { pct: remaining, timeToReserveS: null, source: "fc" };
  }
  return null;
}

export interface AlertInputs {
  status: GsStatus | null;
  telemetry: VehicleState | null;
  battery: BatteryHealth | null;
  /** Milliseconds since vehicle telemetry was last live; null when it has
   *  never been live this session (nothing to have lost). */
  msSinceLive: number | null;
  /** The video state, or null where no video is mounted. */
  video: VideoFeedState | null;
  /** The arm state at the last live sample (the snapshot drops `armed` once
   *  the link goes quiet). Defaults to the snapshot's own value. */
  armedAtLastLive?: boolean | null;
}

const LEVEL_RANK: Record<AlertLevel, number> = { critical: 0, warning: 1, advisory: 2 };

export function computeAlerts(i: AlertInputs): CockpitAlert[] {
  const out: CockpitAlert[] = [];
  const armed = i.telemetry?.armed === true;

  const armedAtLoss = i.armedAtLastLive ?? armed;
  if (i.msSinceLive != null && i.msSinceLive > LINK_LOST_MS && armedAtLoss) {
    out.push({
      id: "link-lost",
      level: "critical",
      title: "Vehicle link lost",
      detail: `No telemetry for ${Math.round(i.msSinceLive / 1000)} s while armed`,
      icon: SignalZero,
    });
  } else if (i.msSinceLive != null && i.msSinceLive > LINK_STALE_MS) {
    out.push({
      id: "link-stale",
      level: "warning",
      title: "Vehicle link stale",
      detail: `Last telemetry ${Math.round(i.msSinceLive / 1000)} s ago`,
      icon: SignalZero,
    });
  }

  const reading = batteryReading(i.battery, i.telemetry);
  if (reading) {
    const reserve = i.battery?.thresholds?.reserve_percent;
    const low = Math.max(BATTERY_LOW_PCT, typeof reserve === "number" ? reserve : 0);
    if (reading.pct <= BATTERY_CRITICAL_PCT) {
      out.push({
        id: "battery",
        level: "critical",
        title: "Battery critical",
        detail: `${Math.round(reading.pct)}% remaining`,
        icon: BatteryWarning,
      });
    } else if (reading.pct <= low) {
      out.push({
        id: "battery",
        level: "warning",
        title: "Battery low",
        detail: `${Math.round(reading.pct)}% remaining`,
        icon: BatteryLow,
      });
    }
  }
  if (i.battery && !i.battery.stale) {
    for (const pack of i.battery.packs) {
      if (pack.stale) continue;
      for (const a of pack.anomalies ?? []) {
        if (a.cleared_at_ms != null) continue;
        if (a.severity !== "critical" && a.severity !== "warning") continue;
        out.push({
          id: `battery-${pack.id}-${a.rule}`,
          level: a.severity,
          title: `Battery ${pack.id + 1}: ${a.rule.replace(/_/g, " ")}`,
          icon: BatteryWarning,
        });
      }
    }
  }

  const fix = i.telemetry?.gps?.fix_type;
  if (armed && typeof fix === "number" && fix < 3) {
    out.push({
      id: "gps",
      level: "warning",
      title: "GPS fix below 3D",
      detail: fix <= 1 ? "No fix" : "2D fix",
      icon: Satellite,
    });
  }

  if (i.video === "frozen") {
    out.push({ id: "video", level: "warning", title: "Video frozen", icon: VideoOff });
  }

  const rf = linkDiagView(i.status?.link?.link_diag);
  if (rf?.actionable) {
    out.push({ id: "rf", level: "warning", title: rf.title, detail: rf.hint, icon: rf.icon });
  }

  const uplinkType = i.status?.network?.uplink_type;
  if (uplinkType && i.status?.network?.uplink_reachable === false) {
    out.push({
      id: "uplink",
      level: "advisory",
      title: "Uplink offline",
      detail: `${uplinkType} is not reachable`,
      icon: WifiOff,
    });
  }

  return out.sort((a, b) => LEVEL_RANK[a.level] - LEVEL_RANK[b.level]);
}
