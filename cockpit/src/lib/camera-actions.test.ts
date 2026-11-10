import { describe, expect, it } from "vitest";

import { cameraRoute, recordRoute, stepDigitalZoom } from "@/lib/camera-actions";
import type { InstalledExtension } from "@/lib/extensions";
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
