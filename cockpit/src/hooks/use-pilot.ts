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

/** Height above ground for the bottom row: the pilot kind's `agl_m` (a fresh
 *  rangefinder, else the EKF height above home), else the follow kind's. */
export function bottomRowAgl(pilot: PilotState | null, followAglM: number | null): number | null {
  return pilot?.aglM ?? followAglM;
}

/** The bottom row's AGL, from the pilot kind with the follow kind behind it. */
export function useBottomRowAgl(): number | null {
  const pilot = useNavStreamStore((s) => s.pilot?.data ?? null);
  const follow = useNavStreamStore((s) => s.follow?.data.aglM ?? null);
  return bottomRowAgl(pilot, follow);
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
      // The layer store stamps frames on the monotonic clock; timeOrigin
      // puts that stamp on the epoch clock `now` reads.
      etaS: nextWaypointEtaS(layerFrames.ar.data, now - (performance.timeOrigin + layerFrames.ar.seen)),
    }),
  );
}
