import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { videoRestartApplies } from "@/lib/reboot-scope";

describe("the pending-reboot banner", () => {
  it("offers a video restart only when every pending change is a video setting", () => {
    expect(videoRestartApplies(["video.camera.fps", "video.encoder.bitrate_kbps"])).toBe(true);
    expect(videoRestartApplies(["video.camera.fps", "network.hotspot.ssid"])).toBe(false);
    expect(videoRestartApplies([])).toBe(false);
  });

  it("reports a failed reboot request instead of swallowing it", () => {
    const src = readFileSync(path.resolve(__dirname, "reboot-banner.tsx"), "utf8");
    expect(src).toContain("toastFromError(err, t(\"diagnostics.reboot.failed\"))");
    expect(src).not.toMatch(/catch \{\s*\/\//);
  });
});
