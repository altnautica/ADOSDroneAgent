// The EKF source sets as the nav-source chip and the minimap read them: the
// nine `EK3_SRC{1,2,3}_{POSXY,VELXY,YAW}` parameters, read once per FC link
// from the parameter cache in one `GET /api/params`, decoded to words, and the chip's input
// assembled from the FC telemetry and the companion's `nav.state`.

import { getParams } from "@/lib/fc-config";
import type { VehicleState } from "@/lib/types";
import { EKF_FLAG, type NavSourceInput } from "@/shared/hud/nav-source";
import type { NavState } from "@/shared/hud/nav-types";
import { specText } from "@/shared/hud/spec-text";
import { useNavStreamStore } from "@/stores/nav-stream-store";

export const EKF_SETS = [1, 2, 3] as const;
export const EKF_AXES = ["POSXY", "VELXY", "YAW"] as const;

/** The parameter names, set-major. */
export const EKF_SOURCE_PARAMS: string[] = EKF_SETS.flatMap((s) => EKF_AXES.map((a) => `EK3_SRC${s}_${a}`));

/** `EK3_SRCn_POSXY` / `_VELXY` value words. */
const XY_WORD: Record<number, string> = {
  0: "None",
  3: "GPS",
  4: "Beacon",
  5: "OpticalFlow",
  6: "ExternalNav",
  7: "WheelEncoder",
};

/** `EK3_SRCn_YAW` value words. */
const YAW_WORD: Record<number, string> = {
  0: "None",
  1: "Compass",
  2: "GPS",
  3: "GPS+Compass",
  6: "ExternalNav",
  8: "GSF",
};

/** The word for one source-set parameter value; `—` when unread. */
export function sourceWord(axis: (typeof EKF_AXES)[number], v: number | undefined): string {
  if (v === undefined) return "—";
  return (axis === "YAW" ? YAW_WORD : XY_WORD)[v] ?? String(v);
}

const FLAG_WORDS: [number, string][] = [
  [EKF_FLAG.VEL_H, "VEL H"],
  [EKF_FLAG.POS_H_REL, "POS H REL"],
  [EKF_FLAG.POS_H_ABS, "POS H ABS"],
  [EKF_FLAG.CONST_POS, "CONST POS"],
  [EKF_FLAG.GPS_GLITCH, "GPS GLITCH"],
];

/** The flag words an EKF report carries, or `—` with no report. */
export function ekfFlagWords(flags: number | null): string {
  if (flags === null) return "—";
  const words = FLAG_WORDS.filter(([bit]) => (flags & bit) !== 0).map(([, w]) => w);
  return words.length > 0 ? words.join(" · ") : "none";
}

/** Read the nine parameters into the store from one `GET /api/params`. A
 *  parameter the FC has not reported (or a failed read) is left out, so the
 *  chip falls back to the flag rules. */
export async function loadEkfSourceParams(signal?: AbortSignal): Promise<void> {
  const all = await getParams(signal).then((r) => r.params ?? {}).catch(() => ({}) as Record<string, number>);
  if (signal?.aborted) return;
  const out: Record<string, number> = {};
  for (const name of EKF_SOURCE_PARAMS) if (typeof all[name] === "number") out[name] = all[name];
  useNavStreamStore.getState().setEkfParams(out);
}

/** The chip's input from the FC telemetry (live or not), the fresh vision-nav
 *  state, the last commanded set and the parameters. An EKF report counts only
 *  while the vehicle telemetry itself is live. */
export function navSourceInput(
  telemetry: VehicleState | null,
  live: boolean,
  nav: NavState | null,
  lastCommandedSet: 1 | 2 | 3 | null,
  params: Record<string, number> | null,
): NavSourceInput {
  const ekf = telemetry?.ekf;
  const gps = telemetry?.gps;
  return {
    ekf: ekf && live ? { flags: ekf.flags, ageMs: ekf.age_ms } : null,
    gps:
      gps && gps.fix_type != null
        ? {
            fixType: gps.fix_type,
            satellites: gps.satellites ?? undefined,
            hdop: gps.eph ?? undefined,
          }
        : null,
    nav,
    lastCommandedSet,
    params: params && Object.keys(params).length > 0 ? params : null,
  };
}

/** The nav-source popover's external-nav row: vision-nav's health, its
 *  odometry rate and the age of its newest pose (`pose_age_ms`), each while
 *  reported. */
export function extNavLine(nav: Pick<NavState, "extnavHealth" | "odomRateHz" | "poseAgeMs"> | null): string {
  const parts = [nav?.extnavHealth ? nav.extnavHealth.toUpperCase() : "—"];
  if (nav?.odomRateHz != null) parts.push(`${nav.odomRateHz.toFixed(1)} Hz`);
  if (nav?.poseAgeMs != null) parts.push(specText("nav_source.pose_age", { ms: Math.round(nav.poseAgeMs) }));
  return parts.join(" · ");
}
