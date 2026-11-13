import { describe, expect, it, vi } from "vitest";

import {
  cameraRoute,
  captureMavPhoto,
  endZoomPresses,
  fetchMavCameras,
  holdMavZoom,
  recordRoute,
  registerCameraActions,
  runCameraAction,
  setMavZoomRate,
  stepDigitalZoom,
  useMavCameraStore,
  ZOOM_HOLD_MAX_MS,
  ZOOM_KEEPALIVE_MS,
  ZOOM_STOP_RETRY_MS,
  ZOOM_STOP_TRIES,
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
  type ZoomBody = { type: string; value: number; keepalive?: boolean };
  /** The camera's answer to a zoom write: a result, after `after` ms, or no
   *  answer from the agent at all (`null`). */
  type Answer = { result: string; after?: number } | null;
  let keepalives: { at: number; body: ZoomBody }[] = [];
  /** Record each zoom write's body and when it went (keepalives apart),
   *  answering `result`, or what `answer` says for that write. */
  function stubZoom(answer: string | ((body: ZoomBody, n: number) => Answer) = "ACCEPTED") {
    const writes: { at: number; body: ZoomBody }[] = [];
    keepalives = [];
    vi.stubGlobal("fetch", async (_url: string, init: RequestInit) => {
      const body = JSON.parse(String(init.body)) as ZoomBody;
      if (body.keepalive) {
        keepalives.push({ at: Date.now(), body });
        return new Response(JSON.stringify({ ok: true, result: "KEEPALIVE", component_id: 100 }), { status: 200 });
      }
      writes.push({ at: Date.now(), body });
      const a = typeof answer === "string" ? { result: answer } : answer(body, writes.length);
      if (a === null) throw new TypeError("network down");
      if (a.after) await new Promise((done) => setTimeout(done, a.after));
      const ok = a.result === "ACCEPTED";
      return new Response(JSON.stringify({ ok, result: a.result, component_id: 100 }), { status: 200, headers: { "Content-Type": "application/json" } });
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

  it("the stop goes at the tap time while the rate still waits for its answer, and again once it comes", () =>
    within(async () => {
      const writes = stubZoom((b) => (b.value === 1 ? { result: "ACCEPTED", after: 4000 } : { result: "ACCEPTED" }));
      const t0 = Date.now();
      holdMavZoom(100, 1)();
      await vi.advanceTimersByTimeAsync(ZOOM_TAP_MS + 10);
      expect(writes.map((w) => [w.at - t0, w.body.value])).toEqual([
        [0, 1],
        [ZOOM_TAP_MS, 0],
      ]);
      await vi.advanceTimersByTimeAsync(4000);
      expect(writes.map((w) => [w.at - t0, w.body.value])).toEqual([
        [0, 1],
        [ZOOM_TAP_MS, 0],
        [4000, 0],
      ]);
    }));

  it("a stop the camera never answered is sent again, a bounded number of times", () =>
    within(async () => {
      for (const lost of [{ result: "TIMEOUT" }, null]) {
        const writes = stubZoom((b) => (b.value === 0 ? lost : { result: "ACCEPTED" }));
        holdMavZoom(100, 1)();
        await vi.advanceTimersByTimeAsync(ZOOM_TAP_MS + ZOOM_STOP_TRIES * ZOOM_STOP_RETRY_MS * 4);
        expect(writes.map((w) => w.body.value)).toEqual([1, ...Array<number>(ZOOM_STOP_TRIES).fill(0)]);
      }
    }));

  it("a stop retry ends at the camera's answer", () =>
    within(async () => {
      const writes = stubZoom((b, n) => (b.value === 0 && n === 2 ? { result: "TIMEOUT" } : { result: "ACCEPTED" }));
      holdMavZoom(100, 1)();
      await vi.advanceTimersByTimeAsync(ZOOM_TAP_MS + ZOOM_STOP_TRIES * ZOOM_STOP_RETRY_MS * 4);
      expect(writes.map((w) => w.body.value)).toEqual([1, 0, 0]);
    }));

  it("a refused stop shows its failure at once and is still sent again", () =>
    within(async () => {
      const seen: ToastMessage[] = [];
      const off = onToast((t) => seen.push(t));
      try {
        const writes = stubZoom((b) => (b.value === 0 ? { result: "DENIED" } : { result: "ACCEPTED" }));
        holdMavZoom(100, 1)();
        await vi.advanceTimersByTimeAsync(ZOOM_TAP_MS + 10);
        expect(seen.map((t) => t.message)).toEqual([specText("camera.zoom_failed", { reason: specText("mav_result.DENIED") })]);
        await vi.advanceTimersByTimeAsync(ZOOM_STOP_TRIES * ZOOM_STOP_RETRY_MS * 4);
        expect(writes.map((w) => w.body.value)).toEqual([1, ...Array<number>(ZOOM_STOP_TRIES).fill(0)]);
        expect(seen).toHaveLength(1);
      } finally {
        off();
      }
    }));

  it("a held zoom keeps the agent's deadman fed until the release, and no longer", () =>
    within(async () => {
      stubZoom();
      const release = holdMavZoom(100, -1);
      await vi.advanceTimersByTimeAsync(ZOOM_KEEPALIVE_MS * 3 + 10);
      expect(keepalives.map((k) => k.body)).toEqual(
        Array(3).fill({ component_id: 100, type: "continuous", value: -1, keepalive: true }),
      );
      expect(ZOOM_KEEPALIVE_MS * 3).toBeLessThan(1000);
      release();
      await vi.advanceTimersByTimeAsync(ZOOM_KEEPALIVE_MS * 5);
      expect(keepalives).toHaveLength(3);
    }));

  it("a held zoom stops when the page hides, the Feed unmounts or the zoom pill goes away", () =>
    within(async () => {
      const doc = Object.assign(new EventTarget(), { visibilityState: "visible" });
      vi.stubGlobal("document", doc);
      vi.stubGlobal("window", new EventTarget());
      const writes = stubZoom();
      const unregister = registerCameraActions(false);
      const ends: [string, () => void][] = [
        ["pagehide", () => window.dispatchEvent(new Event("pagehide"))],
        [
          "hidden",
          () => {
            doc.visibilityState = "hidden";
            document.dispatchEvent(new Event("visibilitychange"));
          },
        ],
        ["pill unmount", endZoomPresses],
        ["feed unmount", unregister],
      ];
      for (const [why, end] of ends) {
        writes.length = 0;
        holdMavZoom(100, 1);
        await vi.advanceTimersByTimeAsync(ZOOM_TAP_MS * 2);
        end();
        await vi.advanceTimersByTimeAsync(ZOOM_TAP_MS);
        expect(writes.map((w) => w.body.value), why).toEqual([1, 0]);
      }
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
