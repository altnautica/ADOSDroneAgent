// The mission stall overlay: while the pilot reports `mission_stall` (a
// mission or a return that stopped making progress, the vehicle holding for
// the operator), every surface shows one persistent critical overlay next to
// its countdowns. A mission stall reads "No progress to waypoint {wp} for
// {window_s} s · Skipping in {in_s} s" (or "· Holding for you" with no
// countdown, or "· No operator · Returning home in {in_s} s" while a
// confined operator wait counts down to SmartRTL) with Skip waypoint, Return and Land here; a blocked return reads
// "Return blocked · Landing in {in_s} s" with Land here only (taking the
// sticks is the other answer). Also the voice line for a newly appeared stall
// or skip, and the skipped waypoints a plan view marks. Pure: imports only
// the spec and its sibling spec-text.

import type { MissionState, MissionStall, PilotState } from "./cockpit-frames";
import { specText } from "./spec-text";

/** The pilot actions a stall overlay offers. */
export type MissionStallAction = "mission_skip" | "rth" | "land";

export interface MissionStallButton {
  action: MissionStallAction;
  labelKey: string;
  label: string;
}

export interface MissionStallOverlay {
  kind: MissionStall["kind"];
  /** Both kinds end in an automatic manoeuvre: critical (`layout.json`
   *  `banners.severity.mission_stalled` / `return_blocked`). */
  severity: "critical";
  titleKey: string;
  titleArgs: Record<string, string | number>;
  title: string;
  actions: MissionStallButton[];
}

const LABEL_KEY: Readonly<Record<MissionStallAction, string>> = {
  mission_skip: "flow.mission_stall.skip",
  rth: "flow.mission_stall.return",
  land: "flow.mission_stall.land",
};

/** The actions a stall offers, in button order. */
export function missionStallActions(kind: MissionStall["kind"]): MissionStallAction[] {
  return kind === "mission" ? ["mission_skip", "rth", "land"] : ["land"];
}

/** The overlay for a stall, or null with none. */
export function missionStallOverlay(stall: MissionStall | null | undefined): MissionStallOverlay | null {
  if (!stall) return null;
  const inS = stall.skipInS === null ? null : Math.max(0, Math.ceil(stall.skipInS));
  const returnInS = stall.returnInS === null ? null : Math.max(0, Math.ceil(stall.returnInS));
  let titleKey: string;
  let titleArgs: Record<string, string | number>;
  if (stall.kind === "mission") {
    titleKey = inS !== null ? "flow.mission_stall.title" : returnInS !== null ? "flow.mission_stall.title_return" : "flow.mission_stall.title_hold";
    titleArgs = { wp: stall.wpLabel ?? (stall.seq !== null ? String(stall.seq) : "—"), window_s: stall.windowS === null ? "—" : Math.round(stall.windowS) };
    if (inS !== null) titleArgs.in_s = inS;
    else if (returnInS !== null) titleArgs.in_s = returnInS;
  } else {
    titleKey = "flow.return_blocked.title";
    titleArgs = { in_s: inS ?? "—" };
  }
  return {
    kind: stall.kind,
    severity: "critical",
    titleKey,
    titleArgs,
    title: specText(titleKey, titleArgs),
    actions: missionStallActions(stall.kind).map((action) => ({ action, labelKey: LABEL_KEY[action], label: specText(LABEL_KEY[action]) })),
  };
}

type VoiceFields = Pick<PilotState, "missionStall" | "banner"> & Partial<Pick<PilotState, "missionDetour" | "returnRoute">>;

/** The skip banners and the voice line each speaks once it newly arrives. */
const SKIP_VOICE: Readonly<Record<string, string>> = {
  "pilot.banner.mission_wp_skipped": "voice.waypoint_skipped",
  "pilot.banner.mission_wp_inside_obstacle": "voice.waypoint_unreachable",
  "pilot.banner.mission_wp_no_path": "voice.waypoint_unreachable",
};

/** The voice key for a change between two pilot states, or null: a stall
 *  that appeared (or changed kind) speaks `voice.mission_stalled` or
 *  `voice.return_blocked`; a detour round a blocked leg that began (or moved
 *  to another item) `voice.mission_detour`; a routed return that began
 *  `voice.return_routed`; a newly arrived skip banner
 *  `voice.waypoint_skipped`, or `voice.waypoint_unreachable` for a waypoint
 *  proven unreachable. */
export function missionStallVoiceKey(prev: VoiceFields | null, next: VoiceFields): string | null {
  const stall = next.missionStall;
  if (stall && stall.kind !== prev?.missionStall?.kind) {
    return stall.kind === "mission" ? "voice.mission_stalled" : "voice.return_blocked";
  }
  if (next.missionDetour && next.missionDetour.seq !== prev?.missionDetour?.seq) return "voice.mission_detour";
  if (next.returnRoute && !prev?.returnRoute) return "voice.return_routed";
  const key = next.banner?.textKey;
  const voice = key ? SKIP_VOICE[key] : undefined;
  if (voice) {
    const was = prev?.banner;
    if (was?.textKey !== key || JSON.stringify(was?.args) !== JSON.stringify(next.banner!.args)) return voice;
  }
  return null;
}

/** A coverage gap's reason as words (`pilot.gap.reason.<reason>`); the bare
 *  code for a reason the spec does not word, null without one. */
export function gapReasonText(reason: string | null): string | null {
  if (!reason) return null;
  const key = `pilot.gap.reason.${reason}`;
  const text = specText(key);
  return text === key ? reason.replace(/_/g, " ") : text;
}

/** The mission items skipped this mission (coverage gaps), ascending and
 *  without repeats: a plan view marks them as skipped. */
export function gapSeqs(mission: Pick<MissionState, "gaps"> | null | undefined): number[] {
  return [...new Set((mission?.gaps ?? []).map((g) => g.seq))].sort((a, b) => a - b);
}

/** The pilot banners a shown stall overlay repeats: the overlay carries the
 *  live countdown, so a surface hides these while it shows. */
const SHADOWED_BANNERS: ReadonlySet<string> = new Set([
  "pilot.banner.mission_stalled",
  "pilot.banner.mission_stalled_hold",
  "pilot.banner.mission_stalled_return",
  "pilot.banner.return_blocked",
]);

/** Whether the banner `textKey` is hidden behind the stall overlay: true for
 *  the stall's own banners while `missionStall` shows. */
export function stallShadowsBanner(textKey: string | null | undefined, missionStall: MissionStall | null | undefined): boolean {
  return !!missionStall && !!textKey && SHADOWED_BANNERS.has(textKey);
}
