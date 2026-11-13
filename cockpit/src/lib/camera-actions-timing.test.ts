// Camera presses answer at once from the cached vehicle camera list, and a
// stalled agent ends in the failure text with the no-answer wording.

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  AGENT_CAMERA_WRITE_BUDGET_MS,
  CAMERA_LIST_TIMEOUT_MS,
  CAMERA_WRITE_TIMEOUT_MS,
  captureMavPhoto,
  fetchMavCameras, frameCrop, runCameraAction,
  setMavZoomRate,
  useMavCameraStore,
} from "@/lib/camera-actions";
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

afterEach(async () => {
  // Let any background list end, so none is shared into the next test.
  await vi.advanceTimersByTimeAsync(CAMERA_LIST_TIMEOUT_MS + 10);
  off();
  vi.unstubAllGlobals();
  vi.useRealTimers();
});

describe("camera timeouts", () => {
  it("the list gives up in 3 to 5 s", () => {
    expect(CAMERA_LIST_TIMEOUT_MS).toBeGreaterThanOrEqual(3000);
    expect(CAMERA_LIST_TIMEOUT_MS).toBeLessThanOrEqual(5000);
  });

  it("a write outlasts the agent's own wait, with margin", () => {
    // probe 1 s + three sends at 1.2 s + the 8 s IN_PROGRESS wait
    expect(AGENT_CAMERA_WRITE_BUDGET_MS).toBe(12_600);
    expect(CAMERA_WRITE_TIMEOUT_MS).toBeGreaterThanOrEqual(AGENT_CAMERA_WRITE_BUDGET_MS + 2000);
  });

  it("a capture the camera is still taking is not cut off before the agent answers", async () => {
    const calls: string[] = [];
    vi.stubGlobal("fetch", (url: string, init: RequestInit = {}) => {
      calls.push(url);
      return new Promise<Response>((resolve, reject) => {
        const t = setTimeout(
          () => resolve(new Response(JSON.stringify({ ok: true, result: "IN_PROGRESS" }), { status: 200, headers: { "Content-Type": "application/json" } })),
          AGENT_CAMERA_WRITE_BUDGET_MS,
        );
        init.signal?.addEventListener("abort", () => (clearTimeout(t), reject(init.signal?.reason)), { once: true });
      });
    });
    const done = captureMavPhoto(101);
    await vi.advanceTimersByTimeAsync(AGENT_CAMERA_WRITE_BUDGET_MS + 10);
    await expect(done).resolves.toEqual({ ok: true, result: "IN_PROGRESS" });
    expect(seen.map((t) => t.message)).toEqual([specText("camera.capture_sent")]);
    expect(calls).toEqual(["/api/v1/camera/mavlink/capture"]);
  });

  it("an IN_PROGRESS answer is never a failure, nor a retry, nor a frame grab", async () => {
    useFeedStore.setState((s) => ({ video: { ...s.video, state: "live" } }));
    useExtensionsStore.setState({ extensions: [] });
    useMavCameraStore.setState({ cameras: [{ component_id: 101, can_zoom: false, can_capture_image: true }], loaded: true });
    const calls: string[] = [];
    vi.stubGlobal("fetch", async (url: string) => {
      calls.push(url);
      const body = url.endsWith("/capture") ? { ok: false, result: "IN_PROGRESS" } : { cameras: [{ component_id: 101, can_zoom: false, can_capture_image: true }] };
      return new Response(JSON.stringify(body), { status: 200, headers: { "Content-Type": "application/json" } });
    });
    const grab = vi.fn();
    vi.stubGlobal("document", { querySelector: grab, createElement: grab });
    await runCameraAction("photo");
    expect(calls.filter((u) => u.endsWith("/capture"))).toHaveLength(1);
    expect(grab).not.toHaveBeenCalled();
    expect(seen.map((t) => t.message)).toEqual([specText("camera.capture_sent")]);
  });

  it("presses share one camera list in flight", async () => {
    const calls: string[] = [];
    vi.stubGlobal("fetch", hangingFetch(calls));
    const store = useMavCameraStore.getState();
    const a = store.refresh();
    const b = store.refresh();
    expect(a).toBe(b);
    await vi.advanceTimersByTimeAsync(CAMERA_LIST_TIMEOUT_MS + 10);
    await a;
    expect(calls).toEqual(["/api/v1/camera/mavlink"]);
    void store.refresh();
    expect(calls).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(CAMERA_LIST_TIMEOUT_MS + 10);
  });

  it("a stalled capture ends in the failure text with the no-answer wording", async () => {
    vi.stubGlobal("fetch", hangingFetch([]));
    const done = captureMavPhoto(101);
    await vi.advanceTimersByTimeAsync(CAMERA_WRITE_TIMEOUT_MS + 10);
    await expect(done).resolves.toBeNull();
    expect(seen.map((t) => t.message)).toEqual([specText("camera.capture_failed", { reason: refusalText("no response") })]);
  });

  it("a stalled zoom ends in the zoom failure text with the no-answer wording", async () => {
    vi.stubGlobal("fetch", hangingFetch([]));
    const done = setMavZoomRate(100, 1);
    await vi.advanceTimersByTimeAsync(CAMERA_WRITE_TIMEOUT_MS + 10);
    await done;
    expect(seen.map((t) => t.message)).toEqual([specText("camera.zoom_failed", { reason: refusalText("no response") })]);
  });

  it("a stalled camera list gives up", async () => {
    vi.stubGlobal("fetch", hangingFetch([]));
    const list = fetchMavCameras();
    await vi.advanceTimersByTimeAsync(CAMERA_LIST_TIMEOUT_MS + 10);
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

  it("crops from the rendered picture: a letterboxed side stays whole until the zoom pushes it off the box", () => {
    // A 4:3 frame on a 16:9 stage: pillarboxed, 960 of 1280 px wide.
    expect(frameCrop(640, 480, 1, 1280, 720)).toEqual({ sx: 0, sy: 0, sw: 640, sh: 480 });
    // At 1.25× the picture is 1200 px wide, still inside the box: the whole width shows.
    const c = frameCrop(640, 480, 1.25, 1280, 720);
    expect(c.sx).toBeCloseTo(0);
    expect(c.sw).toBeCloseTo(640);
    expect(c.sh).toBeCloseTo(384);
    expect(c.sy).toBeCloseTo(48);
    // At 2× it is 1920 wide: 1280 of it shows, two thirds of the frame.
    const d = frameCrop(640, 480, 2, 1280, 720);
    expect(d.sw).toBeCloseTo(640 * (1280 / 1920));
    expect(d.sh).toBeCloseTo(240);
  });
});
