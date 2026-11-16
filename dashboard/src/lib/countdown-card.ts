// The Home countdown card: what the pilot is about to do on its own, with
// the operator's way out. It shows the pilot's banner (spec text, while its
// slot lasts), the low-battery return countdown with Cancel (`rth_cancel`),
// and the persistent mission stall (`mission-stall`: Skip waypoint, Return,
// Land here; Land here only for a blocked return). While the stall shows, its
// own banners stay hidden: the stall carries the live countdown. The
// countdowns read the last polled frame minus its age, so they keep counting
// between polls. Null with nothing to show.

import type { PilotState } from "@/shared/hud/cockpit-frames";
import { countdownS } from "@/shared/hud/format";
import { missionStallOverlay, stallShadowsBanner, type MissionStallOverlay } from "@/shared/hud/mission-stall";
import { hasSpecText, specText } from "@/shared/hud/spec-text";

export interface CountdownButton {
  /** The pilot action the button sends. */
  action: string;
  label: string;
}

export interface CountdownCardModel {
  banner: string | null;
  lowBattery: { title: string; warning: string; cancel: CountdownButton } | null;
  stall: MissionStallOverlay | null;
}

/** A pilot banner in words; a `cause` arg reads through
 *  `pilot.banner.cause.<cause>` (a dash for one the spec does not name). */
export function bannerText(banner: NonNullable<PilotState["banner"]>): string {
  const cause = banner.args.cause;
  if (cause === undefined) return specText(banner.textKey, banner.args);
  const key = `pilot.banner.cause.${typeof cause === "string" ? cause : ""}`;
  return specText(banner.textKey, { ...banner.args, cause: typeof cause === "string" && hasSpecText(key) ? specText(key) : "—" });
}

/** The card for a fresh pilot frame `ageMs` old at `nowMs` (epoch ms). */
export function countdownCard(pilot: PilotState | null, ageMs: number, nowMs: number): CountdownCardModel | null {
  if (!pilot) return null;
  const elapsedS = Math.max(0, ageMs) / 1000;
  const b = pilot.banner;
  const banner = b && (b.untilMs === null || nowMs <= b.untilMs) && !stallShadowsBanner(b.textKey, pilot.missionStall) ? bannerText(b) : null;
  const lowBattery =
    pilot.rthCountdownS !== null
      ? {
          title: specText("flow.low_battery.title", { in_s: countdownS((pilot.rthCountdownS - elapsedS) * 1000) }),
          warning: specText("flow.low_battery.cancel_warning"),
          cancel: { action: "rth_cancel", label: specText("flow.low_battery.cancel") },
        }
      : null;
  const s = pilot.missionStall;
  const stall = missionStallOverlay(
    s && { ...s, skipInS: s.skipInS === null ? null : s.skipInS - elapsedS, returnInS: s.returnInS === null ? null : s.returnInS - elapsedS },
  );
  if (!banner && !lowBattery && !stall) return null;
  return { banner, lowBattery, stall };
}
