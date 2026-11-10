// The pilot state (the `pilot` nav kind, from the nav stream or the pilot
// link) and the flight-state chip derived from it. A cockpit on a node with no
// pilot running reads null and every pilot surface shows its absent state.

import { useState } from "react";

import { useActiveSkillDetail } from "@/hooks/use-follow-skill";
import { useNow } from "@/hooks/use-now";
import { layerFrames } from "@/lib/layer-frames";
import { PILOT_SESSION } from "@/lib/pilot-link";
import { chipInputs } from "@/shared/hud/chip-inputs";
import type { ArFrame, PilotState } from "@/shared/hud/cockpit-frames";
import { CHIP_STALE_AFTER_MS, flightStateChip, type ChipView } from "@/shared/hud/flight-state";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore, type Stamped } from "@/stores/nav-stream-store";

export function usePilot(): Stamped<PilotState> | null {
  return useNavStreamStore((s) => s.pilot);
}

/** Whether a pilot frame received at `at` (epoch ms) is still live: no older
 *  than the spec's `stale_after_ms`, the age at which the chip dims. A frame
 *  past it is never drawn as live (countdowns, gate ring, handover, sticks). */
export function pilotFresh(p: Stamped<PilotState> | null, nowMs: number): boolean {
  return p !== null && nowMs - p.at <= CHIP_STALE_AFTER_MS;
}

/** The pilot state while it is fresh, else null, re-evaluated twice a second
 *  so a stalled pilot ages out with no new frame to re-render it. */
export function useFreshPilot(): PilotState | null {
  const pilot = usePilot();
  const now = useNow(pilot ? 500 : null);
  return pilot !== null && pilotFresh(pilot, now) ? pilot.data : null;
}

/** Whether the stick session belongs to another surface. */
export function sticksElsewhere(p: PilotState | null): { label: string } | null {
  const s = p?.inputSession;
  if (!s) return null;
  if (s.session === null || s.session === PILOT_SESSION) return null;
  return { label: s.label || s.src };
}

/** The newest `ar` frame and its age. The age is read on the monotonic clock
 *  the layer store stamps frames with, so a wall-clock step (a ground station
 *  setting its clock after boot) cannot hide or freeze the ETA. */
export function currentArFrame(): { arFrame: ArFrame | null; arAgeMs: number } {
  return { arFrame: layerFrames.ar.data, arAgeMs: performance.now() - layerFrames.ar.seen };
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
  return flightStateChip(
    chipInputs(pilot.data, now, pilot.at, {
      homeDistM,
      localPos,
      modeDetail: skillDetail,
      completeAgeMs: completeAt === null ? null : now - completeAt,
      ...currentArFrame(),
    }),
  );
}
