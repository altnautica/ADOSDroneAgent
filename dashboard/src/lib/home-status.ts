// The Home status card's read-only facts, from the same shared rules and the
// same inputs the cockpit uses: the status pill and its alert list
// (`status-pill`), the link-loss line, the bottom-row AGL with its source
// mark, who has the sticks, and the battery's time to reserve.

import { aglMarkText, bottomRowAgl } from "@/shared/hud/chip-inputs";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { batteryReading, type BatteryHealthInput } from "@/shared/hud/battery";
import { linkLossLine } from "@/shared/hud/link-loss";
import { specText } from "@/shared/hud/spec-text";
import { baseAlerts, controllerLost, pillAlerts, pillSummary, pilotFailsafe, type PillSummary } from "@/shared/hud/status-pill";

import type { VehicleTelemetry } from "@/hooks/use-vehicle-telemetry";

/** The part of `GET /api/status/full` the pill reads (uplink and storage). */
export interface StatusFull {
  network?: { uplink_type?: string | null; uplink_reachable?: boolean | null } | null;
  system?: { disk_pct?: number | null } | null;
}

export interface HomeStatusInputs {
  pilot: PilotState | null;
  telemetry: VehicleTelemetry | null;
  battery: BatteryHealthInput | null;
  status: StatusFull | null;
  /** Ms since the last telemetry read; null before the first one. */
  msSinceLive: number | null;
  /** Age of the kept pilot frame (its age at the agent plus the time since
   *  receipt); null when none ever arrived. */
  pilotAgeMs: number | null;
  /** False while the agent's own status read fails. */
  agentReachable: boolean;
}

/** The pill and its alert list, from the cockpit's inputs. The dashboard
 *  has no acknowledge action (read-only), so every alert counts as unseen. */
export function homePill(i: HomeStatusInputs): PillSummary {
  const alerts = baseAlerts({
    telemetry: i.telemetry,
    battery: i.battery,
    status: i.status,
    msSinceLive: i.msSinceLive,
    video: null,
  });
  return pillSummary(
    pillAlerts({
      alerts,
      readiness: i.pilot?.readiness ?? [],
      prearm: i.telemetry?.prearm,
      storagePct: i.status?.system?.disk_pct ?? null,
      fcStatus: i.pilot?.statustext,
      pilotAgeMs: i.pilotAgeMs,
      agentReachable: i.agentReachable,
      controllerLost: controllerLost(i.pilot),
      ...pilotFailsafe(i.pilot),
    }),
    new Set(),
  );
}

export interface HomeStatusLines {
  linkLoss: string | null;
  /** `4.1 m` and its source mark (`HOME`, `BARO`), or null with no height. */
  agl: { text: string; mark: string | null } | null;
  /** `Sticks are with <label>`, or null when the pilot reports no input. */
  sticks: string | null;
  /** `m:ss left` to the battery reserve, or null when the engine has none. */
  timeToReserve: string | null;
}

/** `m:ss`, or `h:mm:ss` past an hour (the cockpit's clock format). */
export function fmtClock(seconds: number): string {
  const s = Math.floor(seconds);
  const h = Math.floor(s / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(mm).padStart(2, "0")}:${ss}` : `${mm}:${ss}`;
}

export function homeStatusLines(
  i: HomeStatusInputs & { follow: { aglM: number | null; aglSource: "rng" | "baro" | null } | null },
): HomeStatusLines {
  const agl = bottomRowAgl(i.pilot, i.follow);
  const label = i.pilot?.inputSession?.label ?? i.pilot?.inputSrc ?? null;
  const reading = batteryReading(i.battery, i.telemetry);
  return {
    linkLoss: i.pilot ? linkLossLine(i.pilot) : null,
    agl: agl.m === null ? null : { text: `${agl.m.toFixed(1)} m`, mark: aglMarkText(agl.mark) },
    sticks: label ? specText("flow.sticks_with", { label }) : null,
    timeToReserve:
      reading?.timeToReserveS != null ? specText("cockpit.battery.time_left", { time: fmtClock(reading.timeToReserveS) }) : null,
  };
}
