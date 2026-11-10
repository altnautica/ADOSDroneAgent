// The digital zoom only applies on the stage: the map-view card draws the
// video unzoomed, so a level kept there would be invisible and its label false.

import { useEffect } from "react";

import { useCockpitViewStore } from "@/stores/cockpit-view-store";

/** Reset the digital zoom while the video sits in the card. */
export function syncZoomToPlacement(inCard: boolean): void {
  const view = useCockpitViewStore.getState();
  if (inCard && view.digitalZoom !== 1) view.setDigitalZoom(1);
}

/** Keeps the zoom at 1 while `inCard`, including presses made there. */
export function useZoomResetInCard(inCard: boolean): void {
  const zoom = useCockpitViewStore((s) => s.digitalZoom);
  useEffect(() => syncZoomToPlacement(inCard), [inCard, zoom]);
}
