// The pilot state (the `pilot` nav kind, from the nav stream or the pilot
// link) and the flight-state chip derived from it. A cockpit on a node with no
// pilot running reads null and every pilot surface shows its absent state.

import { useState } from "react";

import { useActiveSkillDetail } from "@/hooks/use-follow-skill";
import { useNow } from "@/hooks/use-now";
import { chipFacts, flightStateChip, type ChipView } from "@/shared/hud/flight-state";
import { layerFrames } from "@/lib/layer-frames";
import { PILOT_SESSION } from "@/lib/pilot-link";
import type { ArFrame, PilotState } from "@/shared/hud/cockpit-frames";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore, type Stamped } from "@/stores/nav-stream-store";

export function usePilot(): Stamped<PilotState> | null {
  return useNavStreamStore((s) => s.pilot);
}

/** The bottom row's height above ground and what it was measured with.
 *  `mark` names a source that is not a ground measurement (the pilot kind's
 *  EKF height above home, the follow kind's barometer), shown beside the
 *  value in the warning colour so it never reads as a measured AGL. */
export interface BottomRowAgl {
  m: number | null;
  mark: "HOME" | "BARO" | null;
}

/** Height above ground for the bottom row: the pilot kind's `agl_m` (a fresh
 *  rangefinder, else the EKF height above home, marked HOME), else the
 *  follow kind's (marked BARO from the barometer). */
export function bottomRowAgl(
  pilot: PilotState | null,
  follow: { aglM: number | null; aglSource: "rng" | "baro" | null } | null,
): BottomRowAgl {
  if (pilot?.aglM != null) return { m: pilot.aglM, mark: pilot.aglSource === "rangefinder" ? null : "HOME" };
  if (follow?.aglM != null) return { m: follow.aglM, mark: follow.aglSource === "baro" ? "BARO" : null };
  return { m: null, mark: null };
}

/** The bottom row's AGL, from the pilot kind with the follow kind behind it. */
export function useBottomRowAgl(): BottomRowAgl {
  const pilot = useNavStreamStore((s) => s.pilot?.data ?? null);
  const followAgl = useNavStreamStore((s) => s.follow?.data.aglM ?? null);
  const followSource = useNavStreamStore((s) => s.follow?.data.aglSource ?? null);
  return bottomRowAgl(pilot, { aglM: followAgl, aglSource: followSource });
}

/** Whether the stick session belongs to another surface. */
export function sticksElsewhere(p: PilotState | null): { label: string } | null {
  const s = p?.inputSession;
  if (!s) return null;
  if (s.session === null || s.session === PILOT_SESSION) return null;
  return { label: s.label || s.src };
}

/** The ETA of the next waypoint in an `ar` frame, from whichever camera
 *  carries it (the agent computes one ETA per waypoint, the same in every
 *  camera). Null when the frame is older than 2 s or names no next waypoint. */
export function nextWaypointEtaS(frame: ArFrame | null, ageMs: number): number | null {
  if (!frame || ageMs > AR_ETA_MAX_AGE_MS) return null;
  for (const id in frame.cameras) {
    for (const it of frame.cameras[id].items) {
      if (it.t === "wp" && it.role === "next" && it.etaS !== null) return it.etaS;
    }
  }
  return null;
}

const AR_ETA_MAX_AGE_MS = 2000;

/** Horizontal distance (m) from the FC's local position to the pilot goal,
 *  both NED about the same origin. Null when either is missing. */
export function goalDistanceM(
  lp: { x: number; y: number } | null | undefined,
  goal: [number, number, number] | null,
): number | null {
  if (!lp || !goal) return null;
  return Math.hypot(goal[0] - lp.x, goal[1] - lp.y);
}

/** The chip for the current pilot state, re-evaluated twice a second so it
 *  dims when the state goes stale. Null with no pilot state at all. */
export function useFlightChip(): ChipView | null {
  const pilot = usePilot();
  const now = useNow(500);
  const homeDistM = useFlightStore((s) => s.telemetry?.home_distance ?? null);
  const localPos = useFlightStore((s) => s.telemetry?.local_position ?? null);
  const skillDetail = useActiveSkillDetail();
  const [completeAt, setCompleteAt] = useState<number | null>(null);
  const complete = pilot?.data.mission?.state === "complete";
  // When the mission turned complete, adjusted while rendering.
  if (complete !== (completeAt !== null)) setCompleteAt(complete ? now : null);
  if (!pilot) return null;
  const detail = pilot.data.owner === "skill" ? skillDetail : null;
  return flightStateChip(
    chipFacts(pilot.data, now - pilot.at, {
      homeDistM,
      modeDetail: detail,
      completeAgeMs: completeAt === null ? null : now - completeAt,
      distM: goalDistanceM(localPos, pilot.data.goalNed),
      etaS: currentArEtaS(),
    }),
  );
}

/** The next waypoint's ETA from the newest `ar` frame. Its age is read on the
 *  monotonic clock the layer store stamps frames with, so a wall-clock step
 *  (a ground station setting its clock after boot) cannot hide or freeze it. */
export function currentArEtaS(): number | null {
  return nextWaypointEtaS(layerFrames.ar.data, performance.now() - layerFrames.ar.seen);
}
