// What the controller tour shows now, read by the tour card and by the
// contextual tips (which wait while it is up).

import { useFreshPilot } from "@/hooks/use-pilot";
import { tourAirborne, tourStage } from "@/lib/controller-tour";
import { useControlsStore } from "@/stores/controls-store";
import { useFlightStore } from "@/stores/flight-store";
import { useInputStore } from "@/stores/input-store";

export function useTourStage(): "none" | "press" | "tour" {
  const padReported = useInputStore((s) => s.pad !== null);
  const announced = useInputStore((s) => s.padAnnounced);
  const seen = useControlsStore((s) => s.tourSeen);
  const pilot = useFreshPilot();
  const telemetryArmed = useFlightStore((s) => s.telemetry?.armed ?? null);
  return tourStage(padReported, announced, seen, tourAirborne(pilot, telemetryArmed));
}
