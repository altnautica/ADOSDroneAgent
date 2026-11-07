// Pure rules of the cockpit's flight flows (C10): the handover banner's
// visibility and its voice prompts, the Launch action's args, the pre-flight
// checklist's pass rule, and the obstacle radar's ring scale. The components
// in components/feed/cockpit render them.

import { BANNERS } from "@/lib/cockpit-spec";
import { cockpitText } from "@/lib/cockpit-text";
import type { PilotState } from "@/shared/hud/cockpit-frames";

/** The handover banner on screen now, or null. A banner with no end time
 *  shows for the spec's handover time from when it first arrived. */
export function visibleBanner(
  banner: PilotState["banner"],
  firstSeenMs: number | null,
  nowMs: number,
): string | null {
  if (!banner) return null;
  const until = banner.untilMs ?? (firstSeenMs !== null ? firstSeenMs + BANNERS.handover_ms : null);
  if (until !== null && nowMs > until) return null;
  return cockpitText(banner.textKey, banner.args);
}

/** The voice prompt for a change between two pilot states, or null. */
export function voicePrompt(prev: PilotState | null, next: PilotState): string | null {
  if (next.banner && next.banner.textKey !== prev?.banner?.textKey) {
    if (next.banner.textKey === "pilot.banner.ados_control" || next.banner.textKey === "pilot.banner.ados_engaged") {
      return cockpitText("voice.ados_control");
    }
    if (next.banner.textKey === "pilot.banner.fc_control") return cockpitText("voice.fc_control");
  }
  if (next.rthCountdownS !== null && prev?.rthCountdownS == null) return cockpitText("voice.low_battery");
  const was = prev?.mission?.state ?? null;
  const now = next.mission?.state ?? null;
  if (now !== was) {
    if (now === "active") return cockpitText("voice.mission_active");
    if (now === "paused") return cockpitText("voice.mission_paused");
    if (now === "complete") return cockpitText("voice.mission_complete");
    if (now === "returning") return cockpitText("voice.rth");
  }
  if (next.owner === "fc_mode" && next.fcModeName && /RTL/i.test(next.fcModeName) && !(prev?.fcModeName && /RTL/i.test(prev.fcModeName))) {
    return cockpitText("voice.rth");
  }
  return null;
}


export type LaunchAfter = "hold" | "skill" | "mission" | "goto";
export type WhenDone = "none" | "land" | "rth";

export const COPTER_TAKEOFF_ALT_M = 3;
export const ALT_MIN_M = 1;
export const ALT_MAX_M = 120;

export interface LaunchPlan {
  after: LaunchAfter;
  altM: number;
  skill: { pluginId: string; skillId: string } | null;
  goto: { n: number; e: number; altM: number } | null;
  whenDone: WhenDone;
}

/** The `launch` action args for a plan (C1/C10). NED down is negative up. */
export function launchArgs(p: LaunchPlan): Record<string, unknown> {
  const args: Record<string, unknown> = { after: p.after, alt_m: p.altM, when_done: p.whenDone === "none" ? null : p.whenDone };
  if (p.after === "skill" && p.skill) args.skill = { plugin_id: p.skill.pluginId, skill_id: p.skill.skillId };
  if (p.after === "goto" && p.goto) args.goto = { n: p.goto.n, e: p.goto.e, d: -p.goto.altM };
  return args;
}


/** Storage above this fill is a blocker (recordings and logs need room). */
export const STORAGE_BLOCK_PCT = 95;
export const STORAGE_WARN_PCT = 85;

export interface ChecklistItem {
  id: string;
  label: string;
  state: "pass" | "warn" | "fail";
}

/** Whether the checklist can be passed: no failing item. */
export function checklistPassable(items: readonly ChecklistItem[]): boolean {
  return items.every((i) => i.state !== "fail");
}


export const RADAR_STALE_MS = 2000;
export const RADAR_RANGE_M = 10;

/** The ring fraction (0 centre .. 1 edge) a sector distance draws at, or null
 *  for no reading; everything past the range clips to the edge. */
export function sectorRadius(d: number | null): number | null {
  if (d === null || !Number.isFinite(d) || d < 0) return null;
  return Math.min(1, d / RADAR_RANGE_M);
}
