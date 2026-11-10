// The page-level banners the shell shows above every framed screen: the agent
// stopped answering, or a drone's flight controller is not connected. Pure so
// the rules are testable. The shell carries no locale, so the copy is here.

import type { AgentProfile } from "@/shared/use-profile";
import type { ReachRefusal } from "@/stores/reach-store";

export type BannerSeverity = "err" | "warn";

export interface ShellBanner {
  id: "agent-offline" | "fc-disconnected";
  severity: BannerSeverity;
  message: string;
}

export interface ShellBannerInputs {
  /** The shell's status poll failed on its last read. */
  statusStale: boolean;
  /** Whether that failure was the agent refusing us; the reach notice owns
   *  that case, so it is not also called "offline". */
  refusal: ReachRefusal;
  profile: AgentProfile | null;
  /** `fc_connected` from the status poll; null or undefined when unknown. */
  fcConnected: boolean | null | undefined;
  /** Whether a status snapshot has ever been read. */
  hasStatus: boolean;
}

export function shellBanners(i: ShellBannerInputs): ShellBanner[] {
  const out: ShellBanner[] = [];
  const offline = i.statusStale && i.refusal === "none";
  if (offline) {
    out.push({ id: "agent-offline", severity: "err", message: "Agent is unreachable. Reconnecting…" });
  }
  // Only on a drone, only while the agent answers (an offline agent says
  // nothing true about its FC), and only on an explicit `false`: an unknown
  // link is never reported as a disconnected one.
  if (i.profile === "drone" && !offline && i.hasStatus && i.fcConnected === false) {
    out.push({ id: "fc-disconnected", severity: "warn", message: "Flight controller is disconnected." });
  }
  return out;
}

/** An `err`/`warn` banner's text: 13 px times the width class's chrome scale
 *  (×1.33 on a television), wrapping to at most two lines, so "Agent is
 *  unreachable. Reconnecting…" reads in full in the narrow top-centre slot. */
export const BANNER_TEXT_PX = 13;
export const BANNER_MAX_LINES = 2;

export function bannerTextStyle(scale: number): {
  fontSize: number;
  lineHeight: string;
  WebkitLineClamp: number;
  display: "-webkit-box";
  WebkitBoxOrient: "vertical";
  overflow: "hidden";
  whiteSpace: "normal";
} {
  return {
    fontSize: BANNER_TEXT_PX * scale,
    lineHeight: `${Math.round(BANNER_TEXT_PX * 1.3 * scale)}px`,
    WebkitLineClamp: BANNER_MAX_LINES,
    display: "-webkit-box",
    WebkitBoxOrient: "vertical",
    overflow: "hidden",
    whiteSpace: "normal",
  };
}
