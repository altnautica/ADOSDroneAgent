// Pure state mapping for the Feed's safety band: reach badge, arm state, GPS,
// link, video, battery and the preflight checks the cockpit can evaluate.
// Unknown is always a dash, never a fabricated value.

import { BATTERY_LOW_PCT, batteryReading } from "@/lib/alerts";
import type { BatteryHealth, GsStatus, VehicleState } from "@/lib/types";
import { DASH, fmtGpsFix } from "@/shared/format";
import type { AgentProfile } from "@/shared/use-profile";
import type { VideoTransportSnapshot } from "@/shared/video-transport";
import type { HomePoint } from "@/stores/flight-store";

export type ReachBadge = "DIRECT" | "LAN" | "VIA GROUND";

/** How the vehicle on screen is reached from this panel: a ground station
 *  shows its relayed aircraft, a drone viewed on-box is direct, and a drone
 *  viewed from another machine is over the LAN. */
export function reachBadge(profile: AgentProfile | null, hostname: string): ReachBadge | null {
  if (profile === "ground_station") return "VIA GROUND";
  if (profile !== "drone") return null;
  return hostname === "localhost" || hostname === "127.0.0.1" || hostname === "::1" ? "DIRECT" : "LAN";
}

export function armLabel(live: boolean, armed: boolean | undefined): string {
  if (!live || armed === undefined) return DASH;
  return armed ? "ARMED" : "DISARMED";
}

/** GPS fix and satellites, e.g. `3D 14`, `RTK fix 21`; a dash when unknown. */
export function gpsLabel(t: VehicleState | null, live: boolean): string {
  if (!live) return DASH;
  const fix = fmtGpsFix(t?.gps?.fix_type);
  const sats = t?.gps?.satellites;
  const satText = typeof sats === "number" && sats <= 200 ? ` ${sats}` : "";
  return fix === DASH ? DASH : `${fix}${satText}`;
}

/** The link chip: heartbeat age on a drone, RSSI on a ground station. */
export function linkLabel(status: GsStatus | null): string {
  const age = status?.heartbeat_age_s;
  if (typeof age === "number" && Number.isFinite(age)) return `HB ${age.toFixed(1)}s`;
  const rssi = status?.link?.rssi_dbm;
  if (typeof rssi === "number" && Number.isFinite(rssi)) return `${Math.round(rssi)} dBm`;
  return DASH;
}

export function videoLabel(v: VideoTransportSnapshot): string {
  switch (v.state) {
    case "live":
      return v.highLatency ? "LIVE · HIGH LAT" : "LIVE";
    case "frozen":
      return "FROZEN";
    case "failed":
      return "NO VIDEO";
    default:
      return "CONNECTING";
  }
}

/** `m:ss`, or `h:mm:ss` past an hour. */
export function fmtClock(seconds: number | null): string {
  if (seconds === null || !Number.isFinite(seconds) || seconds < 0) return DASH;
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(mm).padStart(2, "0")}:${ss}` : `${mm}:${ss}`;
}

/** Battery chip: percent plus time to reserve when the battery engine has it. */
export function batteryLabel(battery: BatteryHealth | null, t: VehicleState | null): string {
  const r = batteryReading(battery, t);
  if (!r) return DASH;
  return r.timeToReserveS !== null ? `${Math.round(r.pct)}% ${fmtClock(r.timeToReserveS)}` : `${Math.round(r.pct)}%`;
}

export interface PreflightCheck {
  id: string;
  label: string;
  ok: boolean;
}

/** The preflight checks this cockpit can evaluate from what the agent
 *  reports. EKF health is not on the telemetry wire, so it is not listed. */
export function preflightChecks(i: {
  telemetry: VehicleState | null;
  battery: BatteryHealth | null;
  live: boolean;
  home: HomePoint | null;
}): PreflightCheck[] {
  const fix = i.telemetry?.gps?.fix_type;
  const reading = batteryReading(i.battery, i.telemetry);
  const reserve = i.battery?.thresholds?.reserve_percent;
  const low = Math.max(BATTERY_LOW_PCT, typeof reserve === "number" ? reserve : 0);
  return [
    { id: "link", label: "Vehicle link fresh", ok: i.live },
    { id: "gps", label: "GPS fix 3D or better", ok: i.live && typeof fix === "number" && fix >= 3 },
    { id: "battery", label: "Battery above the low band", ok: reading !== null && reading.pct > low },
    { id: "home", label: "Home position set", ok: i.home !== null },
  ];
}

/** Seconds since an ISO stamp, or null when absent or unparseable. */
export function secondsSince(iso: string | null | undefined, nowMs: number): number | null {
  if (!iso) return null;
  const t = Date.parse(iso);
  return Number.isFinite(t) ? Math.max(0, (nowMs - t) / 1000) : null;
}
