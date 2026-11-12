// The digital zoom resets while the video sits in the map-view card, which
// draws it unzoomed, and says why.

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { syncZoomToPlacement } from "@/components/feed/video-zoom";
import { onToast } from "@/shared/api/toast";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";

let toasts: string[] = [];
let off: () => void = () => {};

describe("syncZoomToPlacement", () => {
  beforeEach(() => {
    useCockpitViewStore.getState().setDigitalZoom(1);
    toasts = [];
    off = onToast((t) => toasts.push(t.message));
  });
  afterEach(() => off());

  it("resets the zoom when the video moves into the card and says why", () => {
    useCockpitViewStore.getState().setDigitalZoom(4);
    syncZoomToPlacement(true);
    expect(useCockpitViewStore.getState().digitalZoom).toBe(1);
    expect(toasts).toEqual([specText("camera.zoom_off_stage")]);
    expect(toasts[0]).not.toBe("camera.zoom_off_stage");
  });

  it("says nothing with no zoom to reset", () => {
    syncZoomToPlacement(true);
    expect(toasts).toEqual([]);
  });

  it("keeps the zoom on the stage", () => {
    useCockpitViewStore.getState().setDigitalZoom(2);
    syncZoomToPlacement(false);
    expect(useCockpitViewStore.getState().digitalZoom).toBe(2);
    expect(toasts).toEqual([]);
  });
});
