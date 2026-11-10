import { describe, expect, it, vi } from "vitest";

import {
  cameraRoute,
  fetchMavCameras,
  QUALITY_LEASE_S,
  QUALITY_RENEW_MS,
  recordRoute,
  runMavCamera,
  stepDigitalZoom,
  STREAM_QUALITIES,
  streamQualityOf,
  streamQualityRequest,
} from "@/lib/camera-actions";
import type { InstalledExtension } from "@/lib/extensions";
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
      await runMavCamera("photo", 101);
      await runMavCamera("zoom_in", 100);
      await runMavCamera("photo", 101);
    } finally {
      off();
      vi.unstubAllGlobals();
    }
    expect(sent).toEqual([
      { url: "/api/v1/camera/mavlink/capture", body: { component_id: 101 } },
      { url: "/api/v1/camera/mavlink/zoom", body: { component_id: 100, type: "step", value: 1 } },
      { url: "/api/v1/camera/mavlink/capture", body: { component_id: 101 } },
    ]);
    expect(seen.map((t) => [t.kind, t.message])).toEqual([
      ["ok", specText("camera.capture_sent")],
      ["err", specText("camera.zoom_failed", { reason: specText("mav_result.DENIED") })],
      ["err", specText("camera.capture_failed", { reason: "Busy" })],
    ]);
    expect(seen[1].message).toBe("Zoom failed: Denied");
  });

  it("a refused request reads the agent's detail in the failure toast", async () => {
    vi.stubGlobal("fetch", async () => new Response(JSON.stringify({ detail: "no camera" }), { status: 404 }));
    const seen: ToastMessage[] = [];
    const off = onToast((t) => seen.push(t));
    try {
      expect(await runMavCamera("photo", 7)).toBeNull();
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
