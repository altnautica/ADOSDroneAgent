// The Home status card's facts, from the same shared rules and the same
// inputs the cockpit uses: the status pill and its alert list
// (`status-pill`), the link-loss line, the bottom-row AGL with its source
// mark, who has the sticks, the battery's time to reserve, and the follow
// phase with its waiting prompt (`follow-prompt`).
//
// The pill's link items come from vehicle liveness (`useVehicleLiveness`),
// not from whether the agent answered. On the ground it also lists the
// failing pre-flight checks (`@/shared/hud/preflight`). The Home card has no
// video of its own, so it raises no frozen-video item. The lines read the
// pilot and follow frames only while fresh (`freshNavFrames`).
//
// The card is read-mostly: its only actions are the waiting prompt's resume
// and stop, written to the follow-me config (`sendFollowAction`).

import type { NavStatusBody, NavStatusEntry } from "@/hooks/use-nav-status";
import { apiFetch } from "@/shared/api-fetch";
import { BATTERY_FAILSAFE_ID, batteryFailsafeFixable } from "@/shared/hud/battery-failsafe-fix";
import { AIRFRAME_BALANCE_ID, airframeResettable } from "@/shared/hud/liftoff";
import { aglMarkText, bottomRowAgl } from "@/shared/hud/chip-inputs";
import { parsePilotState, type PilotState } from "@/shared/hud/cockpit-frames";
import { batteryReading, type BatteryHealthInput } from "@/shared/hud/battery";
import { linkLossLine } from "@/shared/hud/link-loss";
import { resolveNavSource, type NavSourceResult } from "@/shared/hud/nav-source";
import { PHASE_TONE, type PhaseTone } from "@/shared/hud/follow-chrome";
import { followPrompt, type FollowPromptAction } from "@/shared/hud/follow-prompt";
import { parseFollowState, parseNavState, type FollowPhase, type FollowState, type NavState } from "@/shared/hud/nav-types";
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
  /** The fresh vision-nav state, or null when absent or stale. */
  nav?: NavState | null;
  /** Where the arm state is judged: the fresh pilot frame and live
   *  telemetry only (null when stale or not live). A kept frame may say
   *  disarmed long after the FC armed. Defaults to `pilot` and `telemetry`. */
  armSources?: Pick<HomeStatusInputs, "pilot" | "telemetry">;
}

/** The arm state the pill reads: the pilot frame's (null while the FC is
 *  unheard), else the telemetry's; null while unknown, so the pre-flight
 *  items hide while the drone may be airborne. */
export function pillArmed(i: Pick<HomeStatusInputs, "pilot" | "telemetry">): boolean | null {
  if (i.pilot) return i.pilot.armed;
  return typeof i.telemetry?.armed === "boolean" ? i.telemetry.armed : null;
}

/** The nav source from the live EKF report, GPS and the fresh vision-nav
 *  state (the cockpit chip's inputs, without a commanded set or params). */
export function homeNavSource(i: Pick<HomeStatusInputs, "telemetry" | "live" | "nav" | "pilot">): NavSourceResult {
  const ekf = i.telemetry?.ekf;
  const gps = i.telemetry?.gps;
  return resolveNavSource({
    ekf: ekf && i.live ? { flags: ekf.flags, ageMs: ekf.age_ms } : null,
    gps:
      gps && gps.fix_type != null
        ? { fixType: gps.fix_type, satellites: gps.satellites ?? undefined, hdop: gps.eph ?? undefined }
        : null,
    nav: i.nav ?? null,
    lastCommandedSet: null,
    params: null,
    positionUnverified: i.pilot?.positionUnverified ?? null,
  });
}

/** `Pose age {ms} ms` from the fresh vision-nav state, or null without one. */
export function poseAgeText(nav: NavState | null | undefined): string | null {
  return nav?.poseAgeMs != null ? specText("nav_source.pose_age", { ms: Math.round(nav.poseAgeMs) }) : null;
}

/** The pill alert id that carries the battery failsafe fix (a link to
 *  Aircraft setup that runs the ADOS profile step), or null: the fresh
 *  pilot's battery failsafe item failing because an action does nothing, on
 *  a vehicle known to be disarmed. */
export function batteryFixAlertId(i: Pick<HomeStatusInputs, "pilot" | "telemetry" | "armSources">): string | null {
  const item = i.pilot?.readiness.find((r) => r.id === BATTERY_FAILSAFE_ID);
  return item && batteryFailsafeFixable(item, pillArmed(i.armSources ?? i)) ? `pilot:${BATTERY_FAILSAFE_ID}` : null;
}

/** The pill alert id that carries "I rebalanced it" (the pilot's
 *  `airframe_reset`), or null: the fresh pilot's airframe balance item
 *  warning or blocking, on a vehicle known to be disarmed. */
export function airframeResetAlertId(i: Pick<HomeStatusInputs, "pilot" | "telemetry" | "armSources">): string | null {
  const item = i.pilot?.readiness.find((r) => r.id === AIRFRAME_BALANCE_ID);
  return item && airframeResettable(item, pillArmed(i.armSources ?? i)) ? `pilot:${AIRFRAME_BALANCE_ID}` : null;
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
  const armed = pillArmed(i.armSources ?? i);
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
      setupAbort: i.pilot?.setupAbort ?? null,
      missionStall: i.pilot?.missionStall ?? null,
      preflightFailing: preflightFailing(checks, alerts, armed),
      armed,
      navSource: homeNavSource(i),
    }),
    new Set(),
  );
}

/** The pilot and follow frames of the last nav-status body while each is
 *  fresh at `nowMs` (received at `receivedAtMs` minus its age at the agent),
 *  else null: a dead pilot's kept frame never reads as current. */
export function freshNavFrames(
  body: NavStatusBody | undefined,
  receivedAtMs: number,
  nowMs: number,
): { pilot: PilotState | null; follow: FollowState | null; nav: NavState | null } {
  const fresh = (e: NavStatusEntry | null | undefined): e is NavStatusEntry => !!e && pilotFresh(receivedAtMs - (e.age_ms ?? 0), nowMs);
  return {
    pilot: fresh(body?.pilot) ? parsePilotState(body.pilot.data) : null,
    follow: fresh(body?.follow) ? parseFollowState(body.follow.data) : null,
    nav: fresh(body?.nav) ? parseNavState(body.nav.data) : null,
  };
}

/** The plugin whose per-drone config the follow actions write. */
export const FOLLOW_PLUGIN_ID = "com.altnautica.follow-me";

/** The waiting prompt's actions the Home card offers: the rest need the
 *  cockpit (picking a target) or the sticks' owner (return home, land). */
export type HomeFollowAction = Extract<FollowPromptAction, "resume" | "stop">;

/** The phases a lost target is recovered in, against the search budget. */
const RECOVERY_PHASES: Readonly<Partial<Record<FollowPhase, true>>> = { predicting: true, repositioning: true, searching: true };

export interface HomeFollow {
  /** The phase word and track (`Searching #7`), plus the seconds left of the
   *  search budget while recovering (`Searching #7 9.5 s`). */
  word: string;
  tone: PhaseTone;
  /** The waiting prompt's title and reason, while follow holds for the
   *  operator; else null. */
  prompt: string | null;
  body: string | null;
  actions: HomeFollowAction[];
  /** The resume sequence the follow reports; a resume writes one past it. */
  resumeSeq: number;
}

/** The follow line of a fresh follow frame; null while follow is off or
 *  unheard. */
export function homeFollow(f: FollowState | null): HomeFollow | null {
  if (!f || f.phase === "idle") return null;
  let word = specText(`follow.phase.${f.phase}`);
  if (f.trackId != null) word += ` #${f.trackId}`;
  if (RECOVERY_PHASES[f.phase] && f.reacquireRemainingS != null) word += ` ${f.reacquireRemainingS.toFixed(1)} s`;
  const prompt = followPrompt(f);
  return {
    word,
    tone: PHASE_TONE[f.phase],
    prompt: prompt ? specText(prompt.titleKey) : null,
    body: prompt ? specText(prompt.bodyKey) : null,
    actions: prompt ? prompt.actions.filter((a): a is HomeFollowAction => a === "resume" || a === "stop") : [],
    resumeSeq: f.operatorResumeSeq,
  };
}

/** Send a prompt action through the follow-me config route: resume writes
 *  `operator_resume_seq` one past `resumeSeq`, stop writes `active = false`. */
export function sendFollowAction(action: HomeFollowAction, resumeSeq: number): Promise<unknown> {
  return apiFetch(`/api/plugins/${encodeURIComponent(FOLLOW_PLUGIN_ID)}/config`, {
    method: "PUT",
    body: action === "resume" ? { key: "operator_resume_seq", value: resumeSeq + 1 } : { key: "active", value: false },
  });
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
  // A person or app by its session label only; the raw `input_src` code
  // never reaches the line.
  const label = i.pilot?.inputSession?.label ?? null;
  const reading = batteryReading(i.battery, i.telemetry);
  return {
    linkLoss: i.pilot ? linkLossLine(i.pilot) : null,
    agl: agl.m === null ? null : { text: `${agl.m.toFixed(1)} m`, mark: aglMarkText(agl.mark) },
    sticks: label ? specText("flow.sticks_with", { label }) : null,
    timeToReserve:
      reading?.timeToReserveS != null ? specText("cockpit.battery.time_left", { time: fmtClock(reading.timeToReserveS) }) : null,
  };
}
