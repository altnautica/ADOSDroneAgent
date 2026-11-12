// Pure rules of the cockpit's flight flows: the handover banner's
// visibility and its voice prompts, the confirm holds, the Launch action's
// values and args, the launch sequence, the pre-flight checklist's pass rule,
// the result line of a mode change, and the obstacle radar's ring scale. The
// components in components/feed/cockpit render them.

import { BANNERS } from "@/lib/cockpit-spec";
import layoutJson from "@/shared/cockpit-spec/layout.json";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { countdownS } from "@/shared/hud/format";
import { refusalText } from "@/shared/hud/refusal";
import { hasSpecText, specText } from "@/shared/hud/spec-text";
import { STORAGE_CRIT_PCT, STORAGE_WARN_PCT } from "@/shared/hud/status-pill";

export type HoldId = keyof typeof layoutJson.holds_ms;

/** How long a confirm hold lasts (`layout.json` `holds_ms`). */
export function holdMs(id: HoldId): number {
  return layoutJson.holds_ms[id];
}

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
  const args = "cause" in banner.args ? { ...banner.args, cause: bannerCauseText(banner.args.cause) } : banner.args;
  return specText(banner.textKey, args);
}

/** Why the FC changed mode, in words (`pilot.banner.cause.<cause>`); a
 *  cause this spec does not name reads a dash, never the raw code. */
export function bannerCauseText(cause: unknown): string {
  const key = `pilot.banner.cause.${typeof cause === "string" ? cause : ""}`;
  return typeof cause === "string" && hasSpecText(key) ? specText(key) : "—";
}

export type BannerSeverity = "ok" | "warning" | "critical";

/** A countdown banner's severity (`layout.json` `banners.severity`). */
export function bannerSeverity(id: keyof typeof BANNERS.severity): BannerSeverity {
  return BANNERS.severity[id] as BannerSeverity;
}

/** The low-battery RTH countdown line, whole seconds rounded up. */
export function lowBatteryLine(rthCountdownS: number): string {
  return specText("flow.low_battery.title", { in_s: countdownS(rthCountdownS * 1000) });
}

/** The voice prompt for a change between two pilot states, or null. */
export function voicePrompt(prev: PilotState | null, next: PilotState): string | null {
  if (next.banner && next.banner.textKey !== prev?.banner?.textKey) {
    if (next.banner.textKey === "pilot.banner.ados_control" || next.banner.textKey === "pilot.banner.ados_engaged") {
      return specText("voice.ados_control");
    }
    if (next.banner.textKey === "pilot.banner.fc_control") return specText("voice.fc_control");
  }
  if (next.rthCountdownS !== null && prev?.rthCountdownS == null) return specText("voice.low_battery");
  const was = prev?.mission?.state ?? null;
  const now = next.mission?.state ?? null;
  if (now !== was) {
    if (now === "active") return specText("voice.mission_active");
    if (now === "paused") return specText("voice.mission_paused");
    if (now === "complete") return specText("voice.mission_complete");
    if (now === "returning") return specText("voice.rth");
  }
  if (next.owner === "fc_mode" && next.fcModeName && /RTL/i.test(next.fcModeName) && !(prev?.fcModeName && /RTL/i.test(prev.fcModeName))) {
    return specText("voice.rth");
  }
  return null;
}


export type LaunchAfter = "hold" | "skill" | "mission" | "goto";
export type WhenDone = "none" | "land" | "rth";

/** The Launch sheet's take-off altitude range and default, and the go-to
 *  point it starts with (`layout.json` `launch`; none: the pilot picks it). */
export const LAUNCH = {
  altMinM: layoutJson.launch.alt_min_m,
  altMaxM: layoutJson.launch.alt_max_m,
  altDefaultM: layoutJson.launch.alt_default_m,
  gotoDefault: layoutJson.launch.goto_default as { n: number; e: number } | null,
};

export type LaunchStep = NonNullable<PilotState["sequence"]>["step"];
export const LAUNCH_STEPS: readonly LaunchStep[] = ["checks", "arm", "takeoff", "after", "done"];

/** A launch sequence still on its way: a step before `done`, no error. */
export function launchRunning(seq: PilotState["sequence"]): seq is NonNullable<PilotState["sequence"]> {
  // The chip's `launching` rule (`sequence_running`) reads the same facts.
  return seq != null && seq.step !== "done" && seq.error === null;
}

/** A launch step's name (`flow.launch.step.*`). */
export function launchStepText(step: LaunchStep): string {
  return specText(step === "after" ? "flow.launch.step.after_label" : `flow.launch.step.${step}`);
}

/** A launch step's name as the chip's `launching` rule writes it
 *  ("Taking off · {step}"): the take-off step reads as its action
 *  (`chip.launch_step.takeoff`, "Climb"), so the line matches the chip. */
export function launchStepName(step: LaunchStep): string {
  return step === "takeoff" ? specText("chip.launch_step.takeoff") : launchStepText(step);
}

/** A flight controller's command answer in words (`mav_result.*`), any other
 *  name through the refusal texts. */
export function mavResultText(name: string | null | undefined): string {
  return name && hasSpecText(`mav_result.${name}`) ? specText(`mav_result.${name}`) : refusalText(name);
}

/** The accessible name of a Mode sheet value that needs a hold: avoidance
 *  Off says what it turns off, a control mode what it switches to. */
export function modeHoldLabel(row: string, value: string, label: string): string {
  if (row === "avoid" && value === "off") return specText("flow.mode.avoid_off_hold");
  if (row === "control_mode") return specText("flow.mode.hold_to_switch", { mode: label });
  return specText("flow.mode.fc.hold_aria", { mode: label });
}

/** The Mode sheet's warning under avoidance while it is off, else null. */
export function avoidOffWarning(avoid: string | null): string | null {
  return avoid === "off" ? specText("flow.mode.avoid_off_warning") : null;
}

/** The Mode sheet's result line for one change: `<label>: done`, or the
 *  reason in words. */
export function modeResultText(label: string, r: { ok: boolean; reason?: string }): string {
  return r.ok ? specText("flow.mode.result.done", { label }) : specText("flow.mode.result.failed", { label, reason: r.reason ?? specText("flow.mode.refused") });
}

export interface LaunchPlan {
  after: LaunchAfter;
  altM: number;
  skill: { pluginId: string; skillId: string } | null;
  goto: { n: number; e: number; altM: number } | null;
  whenDone: WhenDone;
}

/** The `launch` action args for a plan (pilot.v1 `launch`). NED down is negative up. */
export function launchArgs(p: LaunchPlan): Record<string, unknown> {
  const args: Record<string, unknown> = { after: p.after, alt_m: p.altM, when_done: p.whenDone === "none" ? null : p.whenDone };
  if (p.after === "skill" && p.skill) args.skill = { plugin_id: p.skill.pluginId, skill_id: p.skill.skillId };
  if (p.after === "goto" && p.goto) args.goto = { n: p.goto.n, e: p.goto.e, d: -p.goto.altM };
  return args;
}


export interface ChecklistItem {
  id: string;
  label: string;
  state: "pass" | "warn" | "fail";
}

/** Whether the checklist can be passed: no failing item. */
export function checklistPassable(items: readonly ChecklistItem[]): boolean {
  return items.every((i) => i.state !== "fail");
}

/** The flight controller's own pre-arm checks: failing while it reported a
 *  PreArm/Arm text in the last 10 s (the newest is shown), passing when it
 *  reported none. Null while the telemetry carries no list (the FC link is
 *  down, which the link item already fails). */
export function fcPrearmItem(prearm: readonly string[] | undefined): ChecklistItem | null {
  if (!Array.isArray(prearm)) return null;
  if (prearm.length === 0) return { id: "fc_prearm", label: specText("flow.preflight.fc_prearm"), state: "pass" };
  const text = prearm[prearm.length - 1].replace(/^(PreArm|Arm):\s*/, "");
  return { id: "fc_prearm", label: specText("flow.preflight.fc_prearm.failing", { text }), state: "fail" };
}

/** On-board storage: fails at or above the spec's critical fill
 *  (`status_pill.storage_crit_pct`), warns at its warning fill, warns when the
 *  node does not report it. */
export function storageItem(pct: number | null): ChecklistItem {
  return {
    id: "storage",
    label: pct === null ? specText("flow.preflight.storage.unknown") : specText("flow.preflight.storage.used", { pct: Math.round(pct) }),
    state: pct === null ? "warn" : pct >= STORAGE_CRIT_PCT ? "fail" : pct >= STORAGE_WARN_PCT ? "warn" : "pass",
  };
}


export const RADAR_STALE_MS = 2000;
export const RADAR_RANGE_M = 10;

/** The ring fraction (0 centre .. 1 edge) a sector distance draws at, or null
 *  for no reading; everything past the range clips to the edge. */
export function sectorRadius(d: number | null): number | null {
  if (d === null || !Number.isFinite(d) || d < 0) return null;
  return Math.min(1, d / RADAR_RANGE_M);
}
