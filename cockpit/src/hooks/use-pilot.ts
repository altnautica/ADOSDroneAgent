// The pilot state (the `pilot` nav kind, from the nav stream or the pilot
// link) and the flight-state chip derived from it. A cockpit on a node with no
// pilot running reads null and every pilot surface shows its absent state.

import { useState } from "react";

import { useActiveSkillDetail } from "@/hooks/use-follow-skill";
import { useNow } from "@/hooks/use-now";
import { flightStateChip, type ChipFacts, type ChipView } from "@/lib/flight-state";
import { PILOT_SESSION } from "@/lib/pilot-link";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore, type Stamped } from "@/stores/nav-stream-store";

export function usePilot(): Stamped<PilotState> | null {
  return useNavStreamStore((s) => s.pilot);
}

/** The first blocking readiness message, or null. */
export function firstBlock(p: PilotState | null): string | null {
  return p?.readiness.find((r) => !r.ok && r.severity === "block")?.msg ?? null;
}

/** Whether the stick session belongs to another surface. */
export function sticksElsewhere(p: PilotState | null): { label: string } | null {
  const s = p?.inputSession;
  if (!s) return null;
  if (s.session === null || s.session === PILOT_SESSION) return null;
  return { label: s.label || s.src };
}

function distance3(a: readonly number[] | null, b: readonly number[] | null): number | null {
  if (!a || !b) return null;
  return Math.hypot(a[0] - b[0], a[1] - b[1]);
}

/** Facts for the chip rules from one pilot state. Exported for tests. */
export function chipFacts(
  p: PilotState,
  ageMs: number,
  extra: { homeDistM: number | null; modeDetail: string | null; completeAgeMs: number | null },
): ChipFacts {
  const block = firstBlock(p);
  return {
    owner: p.owner,
    ready: block === null,
    firstBlockMsg: block,
    setupNeeded: p.readiness.some((r) => r.id === "setup_needed" && !r.ok),
    profile: p.profile,
    skillLabel: p.skill?.label ?? null,
    modeDetail: extra.modeDetail,
    distM: distance3(p.goalNed, p.setpointNed),
    homeDistM: extra.homeDistM,
    setupStep: null,
    missionState: p.mission?.state ?? null,
    seq: p.mission?.seq ?? null,
    total: p.mission?.total ?? null,
    etaS: null,
    landingSeq: p.mission?.landingSeq ?? false,
    completeAgeMs: extra.completeAgeMs,
    vtolTransitioning: false,
    fcMode: p.fcMode,
    fcModeName: p.fcModeName,
    family: p.vehicleClass === "fixedwing" || p.vehicleClass === "vtol" ? "plane" : "copter",
    controlMode: p.controlMode,
    holdReason: p.holdReason,
    avoidState: p.avoidState,
    ageMs,
  };
}

/** The chip for the current pilot state, re-evaluated twice a second so it
 *  dims when the state goes stale. Null with no pilot state at all. */
export function useFlightChip(): ChipView | null {
  const pilot = usePilot();
  const now = useNow(500);
  const homeDistM = useFlightStore((s) => s.telemetry?.home_distance ?? null);
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
    }),
  );
}
