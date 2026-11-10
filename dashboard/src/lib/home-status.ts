// The Home status card's read-only facts, from the same shared rules and the
// same inputs the cockpit uses: the status pill and its alert list
// (`status-pill`), the link-loss line, the bottom-row AGL with its source
// mark, who has the sticks, and the battery's time to reserve.
//
// The pill's link items come from vehicle liveness (`useVehicleLiveness`),
// not from whether the agent answered. On the ground it also lists the
// failing pre-flight checks (`@/shared/hud/preflight`). The Home card has no
// video of its own, so it raises no frozen-video item. The lines read the
// pilot and follow frames only while fresh (`freshNavFrames`).

import type { NavStatusBody, NavStatusEntry } from "@/hooks/use-nav-status";
import { aglMarkText, bottomRowAgl } from "@/shared/hud/chip-inputs";
import { parsePilotState, type PilotState } from "@/shared/hud/cockpit-frames";
import { batteryReading, type BatteryHealthInput } from "@/shared/hud/battery";
import { linkLossLine } from "@/shared/hud/link-loss";
import { parseFollowState } from "@/shared/hud/nav-types";
import { pilotFresh } from "@/shared/hud/pilot-fresh";
import { preflightChecks, preflightFailing } from "@/shared/hud/preflight";
import { specText } from "@/shared/hud/spec-text";
import { storagePct, type StorageStatusFull } from "@/shared/hud/storage";
import { baseAlerts, controllerLost, pillAlerts, pillSummary, pilotFailsafe, type PillSummary } from "@/shared/hud/status-pill";

import type { VehicleTelemetry } from "@/hooks/use-vehicle-telemetry";

/** The part of `GET /api/status/full` the pill reads (uplink and storage:
 *  `health.disk_percent` on a drone, `system.disk_pct` on a ground station). */
export interface StatusFull extends StorageStatusFull {
  network?: { uplink_type?: string | null; uplink_reachable?: boolean | null } | null;
}

export interface HomeStatusInputs {
  pilot: PilotState | null;
  telemetry: VehicleTelemetry | null;
  battery: BatteryHealthInput | null;
  status: StatusFull | null;
  /** Vehicle telemetry is live (attitude present and its stamp moving). */
  live: boolean;
  /** Ms since vehicle telemetry was last live; null when it never was this
   *  session (nothing to have lost). */
  msSinceLive: number | null;
  /** The arm state at the last live sample; null before any. */
  armedAtLastLive: boolean | null;
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
    armedAtLastLive: i.armedAtLastLive,
  });
  const checks = preflightChecks({ telemetry: i.telemetry, battery: i.battery, live: i.live });
  return pillSummary(
    pillAlerts({
      alerts,
      readiness: i.pilot?.readiness ?? [],
      prearm: i.telemetry?.prearm,
      storagePct: storagePct(i.status),
      fcStatus: i.pilot?.statustext,
      pilotAgeMs: i.pilotAgeMs,
      agentReachable: i.agentReachable,
      controllerLost: controllerLost(i.pilot),
      ...pilotFailsafe(i.pilot),
      preflightFailing: preflightFailing(checks, alerts, i.telemetry?.armed),
    }),
    new Set(),
  );
}

type FollowAgl = { aglM: number | null; aglSource: "rng" | "baro" | null };

/** The pilot and follow frames of the last nav-status body while each is
 *  fresh at `nowMs` (received at `receivedAtMs` minus its age at the agent),
 *  else null: a dead pilot's kept frame never reads as current. */
export function freshNavFrames(
  body: NavStatusBody | undefined,
  receivedAtMs: number,
  nowMs: number,
): { pilot: PilotState | null; follow: FollowAgl | null } {
  const fresh = (e: NavStatusEntry | null | undefined): e is NavStatusEntry => !!e && pilotFresh(receivedAtMs - (e.age_ms ?? 0), nowMs);
  return {
    pilot: fresh(body?.pilot) ? parsePilotState(body.pilot.data) : null,
    follow: fresh(body?.follow) ? parseFollowState(body.follow.data) : null,
  };
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
