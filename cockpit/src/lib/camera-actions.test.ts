import { describe, expect, it, vi } from "vitest";

import {
  cameraRoute,
  captureMavPhoto,
  fetchMavCameras,
  holdMavZoom,
  recordRoute,
  runCameraAction,
  setMavZoomRate,
  stepDigitalZoom,
  useMavCameraStore,
  ZOOM_HOLD_MAX_MS,
  ZOOM_TAP_MS,
} from "@/lib/camera-actions";
import type { InstalledExtension } from "@/lib/extensions";
import { QUALITY_LEASE_S, QUALITY_RENEW_MS, STREAM_QUALITIES, streamQualityOf, streamQualityRequest } from "@/lib/stream-quality";
import { onToast, type ToastMessage } from "@/shared/api/toast";
import { specText } from "@/shared/hud/spec-text";

const ext = (pluginId: string, ...local: string[]) =>
  ({ pluginId, skills: local.map((id) => ({ id: `${pluginId}:${id}` })) }) as unknown as InstalledExtension;

describe("the camera chain (extension first, then the local fallback)", () => {
  it("zoom goes to an extension's zoom skill, else to the digital zoom", () => {
    expect(cameraRoute("zoom_in", [ext("pod", "zoom-in")], true)).toEqual({ kind: "skill", skill: "pod:zoom-in" });
    expect(cameraRoute("zoom_in", [], true)).toEqual({ kind: "local" });
    expect(cameraRoute("zoom_out", [ext("pod", "photo")], true)).toEqual({ kind: "local" });
  });

  it("photo goes to an extension's capture skill, else to a local frame grab", () => {
    expect(cameraRoute("photo", [ext("pod", "photo")], true)).toEqual({ kind: "skill", skill: "pod:photo" });
    expect(cameraRoute("photo", [ext("pod", "photo", "capture")], true)).toEqual({ kind: "skill", skill: "pod:capture" });
    expect(cameraRoute("photo", [], true)).toEqual({ kind: "local" });
  });

  it("without video every camera button is unavailable and reads No video", () => {
    const noVideo = { kind: "unavailable", reason: specText("camera.no_video") };
    expect(specText("camera.no_video")).toBe("No video");
    for (const a of ["photo", "zoom_in", "zoom_out"] as const) expect(cameraRoute(a, [ext("pod", "photo", "zoom-in")], false)).toEqual(noVideo);
    // Recentring moves the gimbal and needs no picture.
    expect(cameraRoute("recenter", [ext("pod", "recenter")], false)).toEqual({ kind: "skill", skill: "pod:recenter" });
    expect(recordRoute(false).reason).toBe("No video");
    expect(recordRoute(true).reason).toBeNull();
  });

  it("recentre needs an extension and says so", () => {
    expect(cameraRoute("recenter", [], true)).toEqual({ kind: "unavailable", reason: specText("cockpit.camera.no_extension") });
  });

  it("steps the digital zoom 1× → 2× → 4× and holds at the ends", () => {
    expect(stepDigitalZoom(1, 1)).toBe(2);
    expect(stepDigitalZoom(2, 1)).toBe(4);
    expect(stepDigitalZoom(4, 1)).toBe(4);
    expect(stepDigitalZoom(4, -1)).toBe(2);
    expect(stepDigitalZoom(1, -1)).toBe(1);
    expect(specText("camera.digital_zoom", { n: 2 })).toBe("Digital ×2");
  });
});

describe("the vehicle's own camera over MAVLink (between the extension and the local fallback)", () => {
  const zoomCam = { component_id: 100, can_capture_image: false, can_zoom: true };
  const shotCam = { component_id: 101, can_capture_image: true, can_zoom: false };

  it("zoom: extension, then a camera with can_zoom, then digital", () => {
    expect(cameraRoute("zoom_in", [ext("pod", "zoom-in")], true, [zoomCam])).toEqual({ kind: "skill", skill: "pod:zoom-in" });
    expect(cameraRoute("zoom_in", [], true, [shotCam, zoomCam])).toEqual({ kind: "mavlink", componentId: 100 });
    expect(cameraRoute("zoom_out", [], true, [shotCam])).toEqual({ kind: "local" });
  });

  it("photo: extension, then a camera with can_capture_image, then the frame grab", () => {
    expect(cameraRoute("photo", [ext("pod", "capture")], true, [shotCam])).toEqual({ kind: "skill", skill: "pod:capture" });
    expect(cameraRoute("photo", [], true, [zoomCam, shotCam])).toEqual({ kind: "mavlink", componentId: 101 });
    expect(cameraRoute("photo", [], true, [zoomCam])).toEqual({ kind: "local" });
  });

  it("a vehicle camera works without the shown video", () => {
    expect(cameraRoute("photo", [], false, [shotCam])).toEqual({ kind: "mavlink", componentId: 101 });
    expect(cameraRoute("zoom_in", [], false, [shotCam]).kind).toBe("unavailable");
  });

  it("demo mode lists no vehicle camera and asks the agent nothing", async () => {
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
    vi.stubGlobal("window", { location: { search: "?demo=1" } });
    try {
      expect(await fetchMavCameras()).toEqual([]);
      expect(fetchSpy).not.toHaveBeenCalled();
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it("sends capture and zoom to the agent and toasts the result or the failure", async () => {
    const sent: { url: string; body: unknown }[] = [];
    const replies = [
      { ok: true, result: "ACCEPTED", component_id: 101 },
      { ok: false, result: "DENIED", component_id: 100 },
      { ok: false, result: "TEMPORARILY_REJECTED", statustext: "Busy", component_id: 101 },
    ];
    vi.stubGlobal("fetch", async (url: string, init: RequestInit) => {
      sent.push({ url, body: JSON.parse(String(init.body)) });
      return new Response(JSON.stringify(replies.shift()), { status: 200, headers: { "Content-Type": "application/json" } });
    });
    const seen: ToastMessage[] = [];
    const off = onToast((t) => seen.push(t));
    try {
      await captureMavPhoto(101);
      await setMavZoomRate(100, 1);
      await captureMavPhoto(101);
    } finally {
      off();
      vi.unstubAllGlobals();
    }
    expect(sent).toEqual([
      { url: "/api/v1/camera/mavlink/capture", body: { component_id: 101 } },
      { url: "/api/v1/camera/mavlink/zoom", body: { component_id: 100, type: "continuous", value: 1 } },
      { url: "/api/v1/camera/mavlink/capture", body: { component_id: 101 } },
    ]);
    expect(seen.map((t) => [t.kind, t.message])).toEqual([
      ["ok", specText("camera.capture_sent")],
      ["err", specText("camera.zoom_failed", { reason: specText("mav_result.DENIED") })],
      ["err", specText("camera.capture_failed", { reason: "Busy" })],
    ]);
    expect(seen[1].message).toBe("Zoom failed: Denied");
  });
});

describe("the vehicle camera's zoom (ArduPilot takes a continuous or a range zoom, never a step)", () => {
  /** Record each zoom write's body and when it went, answering `result`. */
  function stubZoom(result = "ACCEPTED") {
    const writes: { at: number; body: { type: string; value: number } }[] = [];
    vi.stubGlobal("fetch", async (_url: string, init: RequestInit) => {
      writes.push({ at: Date.now(), body: JSON.parse(String(init.body)) });
      const ok = result === "ACCEPTED";
      return new Response(JSON.stringify({ ok, result, component_id: 100 }), { status: 200, headers: { "Content-Type": "application/json" } });
    });
    return writes;
  }

  async function within(run: () => Promise<void>) {
    vi.useFakeTimers();
    const off = onToast(() => undefined);
    try {
      await run();
    } finally {
      off();
      vi.useRealTimers();
      vi.unstubAllGlobals();
    }
  }

  it("zooms at full rate from the press and stops at the release", () =>
    within(async () => {
      const writes = stubZoom();
      const t0 = Date.now();
      const release = holdMavZoom(100, 1);
      await vi.advanceTimersByTimeAsync(1500);
      expect(writes.map((w) => w.body)).toEqual([{ component_id: 100, type: "continuous", value: 1 }]);
      release();
      await vi.advanceTimersByTimeAsync(0);
      expect(writes.map((w) => [w.at - t0, w.body.type, w.body.value])).toEqual([
        [0, "continuous", 1],
        [1500, "continuous", 0],
      ]);
    }));

  it("zooms out with a negative rate, and a quick tap still zooms for the tap time", () =>
    within(async () => {
      const writes = stubZoom();
      const t0 = Date.now();
      holdMavZoom(100, -1)();
      await vi.advanceTimersByTimeAsync(ZOOM_TAP_MS + 10);
      expect(writes.map((w) => [w.at - t0, w.body.type, w.body.value])).toEqual([
        [0, "continuous", -1],
        [ZOOM_TAP_MS, "continuous", 0],
      ]);
    }));

  it("a press whose release never comes stops on its own", () =>
    within(async () => {
      const writes = stubZoom();
      holdMavZoom(100, 1);
      await vi.advanceTimersByTimeAsync(ZOOM_HOLD_MAX_MS + 10);
      expect(writes.map((w) => w.body.value)).toEqual([1, 0]);
    }));

  it("a camera that refused the rate gets no stop", () =>
    within(async () => {
      const writes = stubZoom("DENIED");
      holdMavZoom(100, 1)();
      await vi.advanceTimersByTimeAsync(ZOOM_TAP_MS * 2);
      expect(writes.map((w) => w.body.value)).toEqual([1]);
    }));

  it("a zoom press and its release drive the cached vehicle camera", () =>
    within(async () => {
      const writes = stubZoom();
      useMavCameraStore.setState({ cameras: [{ component_id: 100, can_zoom: true, can_capture_image: false }], loaded: true, refresh: async () => [] });
      await runCameraAction("zoom_in");
      await vi.advanceTimersByTimeAsync(800);
      await runCameraAction("zoom_in", "release");
      await vi.advanceTimersByTimeAsync(0);
      expect(writes.map((w) => [w.body.type, w.body.value])).toEqual([
        ["continuous", 1],
        ["continuous", 0],
      ]);
    }));
});

describe("the vehicle camera's photo", () => {
  it("a refused request reads the agent's detail in the failure toast", async () => {
    vi.stubGlobal("fetch", async () => new Response(JSON.stringify({ detail: "no camera" }), { status: 404 }));
    const seen: ToastMessage[] = [];
    const off = onToast((t) => seen.push(t));
    try {
      expect(await captureMavPhoto(7)).toBeNull();
    } finally {
      off();
      vi.unstubAllGlobals();
    }
    expect(seen.map((t) => t.message)).toEqual(["Photo failed: no camera"]);
  });
});

describe("stream quality (the drone's video profile)", () => {
  it("is High while this window holds a hero lease, else Auto", () => {
    expect(streamQualityOf({ leases: [{ holder: "cockpit:1" }] }, "cockpit:1")).toBe("high");
    expect(streamQualityOf({ leases: [{ holder: "gcs-1" }] }, "cockpit:1")).toBe("auto");
    expect(streamQualityOf({}, "cockpit:1")).toBe("auto");
  });

  it("High asks for a hero lease, Auto releases it", () => {
    expect(streamQualityRequest("high", "cockpit:1")).toEqual({
      path: "/api/video/profile",
      method: "POST",
      body: { profile: "hero", holder: "cockpit:1", lease_s: QUALITY_LEASE_S },
    });
    expect(streamQualityRequest("auto", "cockpit:1")).toEqual({ path: "/api/video/profile?holder=cockpit%3A1", method: "DELETE" });
    expect(QUALITY_RENEW_MS).toBeLessThan(QUALITY_LEASE_S * 1000);
  });

  it("labels each choice from the spec", () => {
    expect(STREAM_QUALITIES.map((q) => specText(`camera.quality.${q}`))).toEqual(["Auto", "High"]);
    expect(specText("camera.quality")).toBe("Stream quality");
  });
});
