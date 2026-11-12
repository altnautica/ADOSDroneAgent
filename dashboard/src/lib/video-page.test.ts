import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { assignmentRows, showsLongRangeRadio, videoIdleDetailKey, cameraVideoUrls } from "@/lib/video-page";

const src = (rel: string) => readFileSync(path.resolve(__dirname, "..", rel), "utf8");

describe("the Video page per profile", () => {
  it("hides the long-range radio cards on a visor", () => {
    expect(showsLongRangeRadio("visor")).toBe(false);
    expect(showsLongRangeRadio("drone")).toBe(true);
    const route = src("routes/video-route.tsx");
    expect(route).toContain("const radioCards = showsLongRangeRadio(profile);");
    expect(route).toMatch(/\{radioCards && \(\s*<LinkTuningCard/);
  });

  it("labels the adaptive row as bitrate", () => {
    const route = src("routes/video-route.tsx");
    expect(route).toContain('t("video.fields.adaptiveBitrate")');
    expect(route).not.toContain('t("video.fields.adaptiveFec")');
  });

  it("renders the camera assignments", () => {
    expect(assignmentRows({ thermal: "/dev/video2", main: "/dev/video0", spare: "" })).toEqual([
      { role: "main", device: "/dev/video0" },
      { role: "thermal", device: "/dev/video2" },
    ]);
    expect(src("routes/video-route.tsx")).toContain("assignmentRows(cameras.data?.assignments)");
  });
});

describe("ground-station wording", () => {
  it("waits for the drone instead of asking for a camera", () => {
    expect(videoIdleDetailKey(true)).toBe("video.panel.gsIdle");
    expect(videoIdleDetailKey(false)).toBe("video.panel.plugInCamera");
    expect(src("components/panels/video-panel.tsx")).toContain("t(videoIdleDetailKey(isGround))");
  });

  it("names this node, not always a drone, in the Cloud Relay card", () => {
    const panel = src("components/panels/cloud-panel.tsx");
    expect(panel).toContain('t("panels.cloud.relayDisabledSubject", { subject: nodeSubject(profile) })');
  });
});

describe("cameraVideoUrls", () => {
  const primary = { whepUrl: "/whep", hlsUrl: "/hls/main/index.m3u8" };

  it("keeps the advertised URLs for the primary camera", () => {
    expect(cameraVideoUrls(primary, "main")).toEqual(primary);
  });

  it("switches both transports to the chosen camera's leg", () => {
    expect(cameraVideoUrls(primary, "down 2")).toEqual({
      whepUrl: "/whep?camera=down%202",
      hlsUrl: "/hls/down%202/index.m3u8",
    });
  });

  it("offers no leg for a second camera when the node advertises no video", () => {
    expect(cameraVideoUrls({ whepUrl: "", hlsUrl: "" }, "down")).toEqual({ whepUrl: "", hlsUrl: "" });
  });
});
