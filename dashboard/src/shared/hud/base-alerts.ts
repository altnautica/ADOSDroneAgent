// The base alert rules every surface shares, behind the status pill
// (`status-pill.ts` re-exports everything here): vehicle link, battery, GNSS,
// frozen video, the ground station's re-pair and uplink, a delivered fleet
// slot, and the on-board storage bands the pill reads.
//
// Alert levels:
//   critical: vehicle link lost > 10 s while armed, battery at or below the
//             critical band, a critical battery-engine anomaly
//   warning:  vehicle link stale > 3 s, battery at or below the low band, a
//             GPS fix below 3D while armed, frozen video, a battery-engine
//             warning, the ground station re-pairing because its drone is on
//             the air with a different key, the radio failing to restart onto
//             a delivered fleet slot
//   advisory: the ground station's uplink is offline, a delivered fleet slot
//             waiting for the vehicle to land (never in the pill)
// Every rule reads a real field; an unknown reading never alerts.

import layoutJson from "../cockpit-spec/layout.json";
import { BATTERY_CRITICAL_PCT, batteryLowPct, batteryReading, type BatteryHealthInput, type FcBatteryInput } from "./battery";
import { specText } from "./spec-text";

/** On-board storage use that warns, and that is critical (percent). */
export const STORAGE_WARN_PCT: number = layoutJson.status_pill.storage_warn_pct;
export const STORAGE_CRIT_PCT: number = layoutJson.status_pill.storage_crit_pct;

export const LINK_STALE_MS = 3000;
export const LINK_LOST_MS = 10_000;

export type AlertLevel = "critical" | "warning" | "advisory";

/** One alert. `id` is the spec key suffix (`link_lost`, `batt_low`,
 *  `battery_pack_<pack>_<rule>`, ...), the same on every surface. */
export interface BaseAlert {
  id: string;
  level: AlertLevel;
  title: string;
  detail?: string;
}

export interface BaseAlertInputs {
  /** The telemetry snapshot: arm state, GNSS fix and the FC's battery. */
  telemetry: (FcBatteryInput & { armed?: boolean | null; gps?: { fix_type?: number | null } | null }) | null;
  battery: BatteryHealthInput | null;
  /** Node status: the ground station's uplink and its radio link's re-pair,
   *  and a drone's pending fleet slot (`/api/status/full` `radioFleetApply`). */
  status: {
    network?: { uplink_type?: string | null; uplink_reachable?: boolean | null } | null;
    link?: { pair_rearm?: PairRearm | null } | null;
    radioFleetApply?: RadioFleetApply | null;
  } | null;
  /** Milliseconds since vehicle telemetry was last live; null when it has
   *  never been live this session (nothing to have lost). */
  msSinceLive: number | null;
  /** The video state (`frozen` alerts), or null where no video is mounted. */
  video: string | null;
  /** The arm state at the last live sample (the snapshot drops `armed` once
   *  the link goes quiet). Defaults to the snapshot's own value. */
  armedAtLastLive?: boolean | null;
}

/** The agent's auto-pair re-arm (`pair_rearm` on `GET /api/wfb` and on the
 *  ground-station `/status` `link`): why the key was last re-armed (`unproven`,
 *  `mis_keyed`, `forced`) and whether the bind window it opened still runs.
 *  Null when the key on disk was never re-armed. */
export interface PairRearm {
  cause?: string | null;
  active?: boolean | null;
  at?: number | null;
  episodes?: number | null;
}

/** A re-pair is running because the peer is on the air with a different key:
 *  on a ground station, the `gs_repairing` alert. */
export function repairingMisKeyed(r: PairRearm | null | undefined): boolean {
  return r?.active === true && r.cause === "mis_keyed";
}

/** A fleet address delivered to the vehicle and the radio restart it is owed
 *  (`radioFleetApply` on `/api/status/full`, `radio.fleet_apply` in the cloud
 *  heartbeat): `outcome` is `pending`, `deferred_armed` (waits for the vehicle
 *  to disarm), `applied` or `restart_failed` (retried). */
export interface RadioFleetApply {
  outcome?: string | null;
  fleetId?: number | null;
  fleetSlot?: number | null;
}

/** The full status's `radioFleetApply`, or null when absent or not an
 *  object; only the typed fields pass. */
export function radioFleetApplyOf(v: unknown): RadioFleetApply | null {
  if (!v || typeof v !== "object" || Array.isArray(v)) return null;
  const r = v as Record<string, unknown>;
  return {
    outcome: typeof r.outcome === "string" ? r.outcome : null,
    fleetId: Number.isFinite(r.fleetId) ? (r.fleetId as number) : null,
    fleetSlot: Number.isFinite(r.fleetSlot) ? (r.fleetSlot as number) : null,
  };
}

/** The pending fleet slot as an alert: `fleet_slot_pending` (advisory) while
 *  the delivered slot waits for the vehicle to land, `fleet_slot_failed`
 *  (warning) while the radio restart onto it fails; null otherwise. */
export function fleetSlotAlert(r: RadioFleetApply | null | undefined): BaseAlert | null {
  const slot = r?.fleetSlot;
  if (typeof slot !== "number") return null;
  if (r?.outcome === "deferred_armed") {
    return { id: "fleet_slot_pending", level: "advisory", title: specText("alert.fleet_slot_pending", { slot }) };
  }
  if (r?.outcome === "restart_failed") {
    return { id: "fleet_slot_failed", level: "warning", title: specText("alert.fleet_slot_failed", { slot }) };
  }
  return null;
}

const LEVEL_RANK: Record<AlertLevel, number> = { critical: 0, warning: 1, advisory: 2 };

/** Alerts worst first; equal levels keep their order. */
export function sortAlerts<T extends { level: AlertLevel }>(alerts: T[]): T[] {
  return alerts.sort((a, b) => LEVEL_RANK[a.level] - LEVEL_RANK[b.level]);
}

/** The alert rules every surface shares, worst first. */
export function baseAlerts(i: BaseAlertInputs): BaseAlert[] {
  const out: BaseAlert[] = [];
  const armed = i.telemetry?.armed === true;

  const armedAtLoss = i.armedAtLastLive ?? armed;
  if (i.msSinceLive != null && i.msSinceLive > LINK_LOST_MS && armedAtLoss) {
    const s = Math.round(i.msSinceLive / 1000);
    out.push({ id: "link_lost", level: "critical", title: specText("alert.link_lost"), detail: specText("alert.link_lost.detail", { s }) });
  } else if (i.msSinceLive != null && i.msSinceLive > LINK_STALE_MS) {
    const s = Math.round(i.msSinceLive / 1000);
    out.push({ id: "link_stale", level: "warning", title: specText("alert.link_stale"), detail: specText("alert.link_stale.detail", { s }) });
  }

  const reading = batteryReading(i.battery, i.telemetry);
  if (reading) {
    const detail = specText("alert.battery.detail", { pct: Math.round(reading.pct) });
    if (reading.pct <= BATTERY_CRITICAL_PCT) {
      out.push({ id: "batt_crit", level: "critical", title: specText("alert.batt_crit"), detail });
    } else if (reading.pct <= batteryLowPct(i.battery)) {
      out.push({ id: "batt_low", level: "warning", title: specText("alert.batt_low"), detail });
    }
  }
  if (i.battery && !i.battery.stale) {
    for (const pack of i.battery.packs) {
      if (pack.stale) continue;
      for (const a of pack.anomalies ?? []) {
        if (a.cleared_at_ms != null) continue;
        if (a.severity !== "critical" && a.severity !== "warning") continue;
        out.push({
          id: `battery_pack_${pack.id}_${a.rule}`,
          level: a.severity,
          title: specText("alert.battery_pack", { pack: pack.id + 1, rule: a.rule.replace(/_/g, " ") }),
        });
      }
    }
  }

  const fix = i.telemetry?.gps?.fix_type;
  if (armed && typeof fix === "number" && fix < 3) {
    out.push({
      id: "gps_no_3d",
      level: "warning",
      title: specText("alert.gps_no_3d"),
      detail: specText(fix <= 1 ? "alert.gps_no_3d.no_fix" : "alert.gps_no_3d.fix_2d"),
    });
  }

  if (i.video === "frozen") {
    out.push({ id: "video_frozen", level: "warning", title: specText("alert.video_frozen") });
  }

  if (repairingMisKeyed(i.status?.link?.pair_rearm)) {
    out.push({ id: "gs_repairing", level: "warning", title: specText("alert.gs_repairing") });
  }

  const fleetSlot = fleetSlotAlert(i.status?.radioFleetApply);
  if (fleetSlot) out.push(fleetSlot);

  const uplinkType = i.status?.network?.uplink_type;
  if (uplinkType && i.status?.network?.uplink_reachable === false) {
    out.push({ id: "uplink", level: "advisory", title: specText("alert.uplink"), detail: specText("alert.uplink.detail", { type: uplinkType }) });
  }

  return sortAlerts(out);
}
