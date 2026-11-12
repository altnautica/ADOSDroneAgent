// The digital zoom only applies on the stage: the map-view card draws the
// video unzoomed, so a level kept there would be invisible and its label false.
// A zoom reset there says why (`camera.zoom_off_stage`), never silently.

import { useEffect } from "react";

import { toast } from "@/shared/api/toast";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";

/** Reset the digital zoom while the video sits in the card, saying why. */
export function syncZoomToPlacement(inCard: boolean): void {
  const view = useCockpitViewStore.getState();
  if (!inCard || view.digitalZoom === 1) return;
  view.setDigitalZoom(1);
  toast.info(specText("camera.zoom_off_stage"));
}

/** Keeps the zoom at 1 while `inCard`, including presses made there. */
export function useZoomResetInCard(inCard: boolean): void {
  const zoom = useCockpitViewStore((s) => s.digitalZoom);
  useEffect(() => syncZoomToPlacement(inCard), [inCard, zoom]);
}
