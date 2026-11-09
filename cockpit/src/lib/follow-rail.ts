// The following-mode chrome's pure rules (13-hud-v2 §8.2, §8.4, §11): which
// right-rail buttons a width class shows and how big they are, the follow
// button's word, a plugin setting's pending/echo state (STRICT | SMART,
// FOLLOW | ORBIT, a slider value), the pilot's avoidance chip, the active
// skill's chip detail (`ORBIT 8 m`), the rail's bottom clearance over the
// radar, and the re-acquired toast text. DOM-free, so every rule is
// unit-tested.

import { BOTTOM_CHROME_CLEARANCE_PX, GLOVE_TOUCH_SCALE, type WidthClass } from "@/lib/cockpit-layout";
import { cockpitText } from "@/lib/cockpit-text";
import type { ParamValue } from "@/lib/skills";
import type { AvoidState } from "@/shared/hud/cockpit-frames";
import type { FollowEvent, FollowPhase } from "@/shared/hud/nav-types";

export type RailButton = "follow" | "pattern" | "mode" | "camSwap" | "rec";

/** A setting write with no echo after this reverts with a warning (§8.2 #2). */
export const SETTING_ECHO_TIMEOUT_MS = 3000;
/** How long the re-acquired toast stays (advisory). */
export const REACQUIRED_TOAST_MS = 4000;
/** Below this safe height the compact rail folds the camera buttons into `⋯`. */
export const RAIL_FOLD_HEIGHT_PX = 360;
/** Gap between rail buttons. */
export const RAIL_GAP_PX = 6;

/** The radar's edge per width class (§11), which the rail keeps clear of. */
export const RADAR_PX: Record<WidthClass, number> = { compact: 88, medium: 100, expanded: 140, tv: 176 };

/** Rail button edge per width class, ×1.25 in glove mode. */
export function railButtonPx(width: WidthClass, glove: boolean): number {
  const base = width === "tv" ? 64 : width === "expanded" ? 56 : 48;
  return glove ? base * GLOVE_TOUCH_SCALE : base;
}

export interface RailInput {
  width: WidthClass;
  /** A follow-type toggle skill is installed. */
  followInstalled: boolean;
  /** The follow skill carries the FOLLOW | ORBIT pattern (`follow_mode`). */
  hasPattern: boolean;
  /** The follow skill carries the re-acquire setting. */
  hasModeSetting: boolean;
  /** A second concurrent leg is in the PiP to swap with. */
  canSwap: boolean;
  /** The Feed region's height, for the compact fold. */
  safeHeightPx: number;
}

export interface RailLayout {
  /** Buttons shown directly, top to bottom. */
  buttons: RailButton[];
  /** Buttons behind the `⋯` button (compact, short panels). */
  folded: RailButton[];
}

/** The rail for a width class: the follow group and the camera buttons.
 *  Hold, return home and land live in the left column only; the rail never
 *  repeats them. */
export function railLayout(i: RailInput): RailLayout {
  const top: RailButton[] = [];
  if (i.followInstalled) top.push("follow");
  if (i.followInstalled && i.hasPattern) top.push("pattern");
  if (i.followInstalled && i.hasModeSetting) top.push("mode");
  const extra: RailButton[] = [];
  if (i.canSwap) extra.push("camSwap");
  extra.push("rec");
  const fold = i.width === "compact" && i.safeHeightPx < RAIL_FOLD_HEIGHT_PX;
  return {
    buttons: [...top, ...(fold ? [] : extra)],
    folded: fold ? extra : [],
  };
}

/** The rail's bottom clearance: above the skill picker, and above the radar
 *  where the radar sits in the bottom-right corner (compact, medium). At
 *  expanded and tv the radar sits left of the rail, so only the picker counts. */
export function railBottomPx(width: WidthClass, hasRadar: boolean): number {
  const radar = hasRadar && (width === "compact" || width === "medium") ? RADAR_PX[width] + 8 : 0;
  return BOTTOM_CHROME_CLEARANCE_PX + radar;
}

/** The follow button: `FOLLOW` while idle with a target designated, `TRACKING`
 *  (with ×) while active; hidden otherwise. */
export function followButton(
  active: boolean,
  phase: FollowPhase | null,
  designated: boolean,
): "follow" | "tracking" | null {
  if (active || (phase !== null && phase !== "idle")) return "tracking";
  return designated ? "follow" : null;
}

export interface SettingDisplayInput {
  /** The value the plugin echoes it runs with, or null. */
  echo: ParamValue | null;
  /** The configured value or the manifest default, shown before any echo. */
  fallback: ParamValue | null;
  /** The value this cockpit last wrote, while unconfirmed. */
  requested: ParamValue | null;
  requestedAt: number | null;
  now: number;
}

export interface SettingDisplay {
  value: ParamValue | null;
  /** Written, not yet echoed: drawn with a dashed outline. */
  pending: boolean;
  /** Written and never echoed within the timeout: revert and warn. */
  expired: boolean;
  /** The echo matched the request: the request can be dropped. */
  confirmed: boolean;
}

/** A setting's shown value: the request while pending, the echo once it
 *  matches, and the echo (or fallback) again when no echo came in 3 s. */
export function settingDisplay(i: SettingDisplayInput): SettingDisplay {
  const applied = i.echo ?? i.fallback;
  if (i.requested === null) return { value: applied, pending: false, expired: false, confirmed: false };
  if (i.echo === i.requested) return { value: i.echo, pending: false, expired: false, confirmed: true };
  if (i.requestedAt !== null && i.now - i.requestedAt > SETTING_ECHO_TIMEOUT_MS) {
    return { value: applied, pending: false, expired: true, confirmed: false };
  }
  return { value: i.requested, pending: true, expired: false, confirmed: false };
}

/** The pilot's avoidance state as the follow controls' chip. */
export function avoidChip(state: AvoidState | null): { text: string; tone: "ok" | "warn" | "crit" } | null {
  switch (state) {
    case "clear":
      return { text: "CLEAR", tone: "ok" };
    case "detour":
      return { text: "AVOIDING", tone: "warn" };
    case "blocked":
      return { text: "BLOCKED", tone: "crit" };
    case "off":
      return { text: cockpitText("pilot.avoid.off"), tone: "crit" };
    default:
      return null;
  }
}

/** The chip's mode detail for a running skill from its echoed parameters:
 *  follow-me's `ORBIT 8 m` / `FOLLOW 6 m`; null when nothing is known. */
export function skillDetail(echo: Readonly<Record<string, ParamValue>> | undefined): string | null {
  if (!echo) return null;
  const metres = (v: ParamValue | undefined) => (typeof v === "number" ? ` ${Math.round(v * 10) / 10} m` : "");
  if (echo.follow_mode === "orbit") return `ORBIT${metres(echo.orbit_radius_m)}`;
  if (echo.follow_mode === "follow") return `FOLLOW${metres(echo.follow_distance_m)}`;
  return null;
}

/** `Re-acquired #15 · 0.86 · 1.8 s gap` for a `follow.reacquired` event. */
export function reacquiredToastText(ev: FollowEvent): string | null {
  if (ev.topic !== "follow.reacquired") return null;
  return `Re-acquired #${ev.trackId} · ${ev.similarity.toFixed(2)} · ${ev.gapS.toFixed(1)} s gap`;
}
