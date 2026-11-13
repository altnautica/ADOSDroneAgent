// What the controller tour shows now, read by the tour card and by the
// contextual tips (which wait while it is up), and whether the card may
// capture the pad.

import { useFreshPilot } from "@/hooks/use-pilot";
import { liveArmed, tourAirborne, tourCapturesPad, tourStage } from "@/lib/controller-tour";
import { useControlsStore } from "@/stores/controls-store";
import { useFlightStore } from "@/stores/flight-store";
import { useInputStore } from "@/stores/input-store";

export function useTourStage(): "none" | "press" | "tour" {
  const padReported = useInputStore((s) => s.pad !== null);
  const announced = useInputStore((s) => s.padAnnounced);
  const seen = useControlsStore((s) => s.tourSeen);
  const pilot = useFreshPilot();
  const telemetryArmed = useFlightStore(liveArmed);
  return tourStage(padReported, announced, seen, tourAirborne(pilot, telemetryArmed));
}

/** Whether the tour card may capture the pad: only on a vehicle known to be on
 *  the ground and disarmed (see `tourCapturesPad`). */
export function useTourCapturesPad(): boolean {
  const pilot = useFreshPilot();
  const live = useFlightStore((s) => s.live && !s.stale);
  const armed = useFlightStore((s) => s.telemetry?.armed ?? null);
  return tourCapturesPad(pilot, { live, armed });
}
