// The digital zoom resets while the video sits in the map-view card, which
// draws it unzoomed.

import { beforeEach, describe, expect, it } from "vitest";

import { syncZoomToPlacement } from "@/components/feed/video-zoom";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";

describe("syncZoomToPlacement", () => {
  beforeEach(() => useCockpitViewStore.getState().setDigitalZoom(1));

  it("resets the zoom when the video moves into the card", () => {
    useCockpitViewStore.getState().setDigitalZoom(4);
    syncZoomToPlacement(true);
    expect(useCockpitViewStore.getState().digitalZoom).toBe(1);
  });

  it("keeps the zoom on the stage", () => {
    useCockpitViewStore.getState().setDigitalZoom(2);
    syncZoomToPlacement(false);
    expect(useCockpitViewStore.getState().digitalZoom).toBe(2);
  });
});
