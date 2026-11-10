// Camera presses answer at once from the cached vehicle camera list, and a
// stalled agent ends in the failure text with the no-answer wording.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { CAMERA_TIMEOUT_MS, fetchMavCameras, frameCrop, runCameraAction, runMavCamera, useMavCameraStore } from "@/lib/camera-actions";
import { onToast, type ToastMessage } from "@/shared/api/toast";
import { refusalText } from "@/shared/hud/refusal";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFeedStore } from "@/stores/feed-store";

/** A fetch that never answers until its request is aborted. */
function hangingFetch(calls: string[]) {
  return (url: string, init: RequestInit = {}) => {
    calls.push(url);
    return new Promise<Response>((_, reject) => {
      init.signal?.addEventListener("abort", () => reject(init.signal?.reason ?? new Error("aborted")), { once: true });
    });
  };
}

let seen: ToastMessage[] = [];
let off: () => void = () => {};

beforeEach(() => {
  vi.useFakeTimers();
  seen = [];
  off = onToast((t) => seen.push(t));
  vi.stubGlobal("window", { location: { search: "" } });
});

afterEach(() => {
  off();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("camera timeouts", () => {
  it("is 3 to 5 s", () => {
    expect(CAMERA_TIMEOUT_MS).toBeGreaterThanOrEqual(3000);
    expect(CAMERA_TIMEOUT_MS).toBeLessThanOrEqual(5000);
  });

  it("a stalled capture ends in the failure text with the no-answer wording", async () => {
    vi.stubGlobal("fetch", hangingFetch([]));
    const done = runMavCamera("photo", 101);
    await vi.advanceTimersByTimeAsync(CAMERA_TIMEOUT_MS + 10);
    await expect(done).resolves.toBeNull();
    expect(seen.map((t) => t.message)).toEqual([specText("camera.capture_failed", { reason: refusalText("no response") })]);
  });

  it("a stalled zoom ends in the zoom failure text with the no-answer wording", async () => {
    vi.stubGlobal("fetch", hangingFetch([]));
    const done = runMavCamera("zoom_in", 100);
    await vi.advanceTimersByTimeAsync(CAMERA_TIMEOUT_MS + 10);
    await done;
    expect(seen.map((t) => t.message)).toEqual([specText("camera.zoom_failed", { reason: refusalText("no response") })]);
  });

  it("a stalled camera list gives up", async () => {
    vi.stubGlobal("fetch", hangingFetch([]));
    const list = fetchMavCameras();
    await vi.advanceTimersByTimeAsync(CAMERA_TIMEOUT_MS + 10);
    await expect(list).resolves.toEqual([]);
  });
});

describe("a camera press routes on the cached list", () => {
  beforeEach(() => {
    useFeedStore.setState((s) => ({ video: { ...s.video, state: "live" } }));
    useExtensionsStore.setState({ extensions: [] });
    useCockpitViewStore.getState().setDigitalZoom(1);
  });

  it("steps the digital zoom without waiting for the vehicle's camera list", async () => {
    const calls: string[] = [];
    vi.stubGlobal("fetch", hangingFetch(calls));
    useMavCameraStore.setState({ cameras: [], loaded: true });
    await runCameraAction("zoom_in");
    expect(useCockpitViewStore.getState().digitalZoom).toBe(2);
    // The list still refreshes, in the background.
    expect(calls).toEqual(["/api/v1/camera/mavlink"]);
  });

  it("sends a zoom to the cached vehicle camera", async () => {
    const calls: string[] = [];
    vi.stubGlobal("fetch", async (url: string) => {
      calls.push(url);
      const body = url.endsWith("/zoom") ? { ok: true, result: "ACCEPTED" } : { cameras: [{ component_id: 100, can_zoom: true, can_capture_image: false }] };
      return new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } });
    });
    useMavCameraStore.setState({ cameras: [{ component_id: 100, can_zoom: true, can_capture_image: false }], loaded: true });
    await runCameraAction("zoom_in");
    expect(calls).toContain("/api/v1/camera/mavlink/zoom");
    expect(useCockpitViewStore.getState().digitalZoom).toBe(1);
  });

  it("keeps the cached list when a refresh fails", async () => {
    const cams = [{ component_id: 100, can_zoom: true, can_capture_image: true }];
    useMavCameraStore.setState({ cameras: cams, loaded: true });
    vi.stubGlobal("fetch", async () => new Response("down", { status: 502 }));
    await useMavCameraStore.getState().refresh();
    expect(useMavCameraStore.getState().cameras).toEqual(cams);
  });
});

describe("the frame grab at a digital zoom", () => {
  it("saves the centre crop the pilot sees", () => {
    expect(frameCrop(1280, 720, 1)).toEqual({ sx: 0, sy: 0, sw: 1280, sh: 720 });
    expect(frameCrop(1280, 720, 2)).toEqual({ sx: 320, sy: 180, sw: 640, sh: 360 });
    expect(frameCrop(1280, 720, 4)).toEqual({ sx: 480, sy: 270, sw: 320, sh: 180 });
  });
});
