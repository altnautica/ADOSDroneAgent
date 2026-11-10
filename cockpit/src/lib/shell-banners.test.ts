import { describe, expect, it } from "vitest";

import { shouldShowCloudNudge } from "@/hooks/use-cloud-posture-nudge";
import { gridMeasures } from "@/lib/cockpit-layout";
import { BANNER_MAX_LINES, BANNER_TEXT_PX, bannerTextStyle, shellBanners, type ShellBannerInputs } from "@/lib/shell-banners";

const base: ShellBannerInputs = {
  statusStale: false,
  refusal: "none",
  profile: "drone",
  fcConnected: true,
  hasStatus: true,
};

describe("shell banners", () => {
  it("shows nothing when the agent answers and the FC is linked", () => {
    expect(shellBanners(base)).toEqual([]);
  });

  it("reports a failed status poll as agent offline", () => {
    expect(shellBanners({ ...base, statusStale: true }).map((b) => b.id)).toEqual(["agent-offline"]);
  });

  it("leaves a refusal to the reach notice", () => {
    expect(shellBanners({ ...base, statusStale: true, refusal: "unpaired" })).toEqual([]);
  });

  it("reports a disconnected FC on a drone only", () => {
    expect(shellBanners({ ...base, fcConnected: false }).map((b) => b.id)).toEqual(["fc-disconnected"]);
    expect(shellBanners({ ...base, profile: "ground_station", fcConnected: false })).toEqual([]);
  });

  it("never reports an unknown FC link, or one from an offline agent", () => {
    expect(shellBanners({ ...base, fcConnected: undefined })).toEqual([]);
    expect(shellBanners({ ...base, fcConnected: false, hasStatus: false })).toEqual([]);
    expect(shellBanners({ ...base, fcConnected: false, statusStale: true }).map((b) => b.id)).toEqual(["agent-offline"]);
  });
});

describe("cloud posture nudge", () => {
  it("shows only on the cloud posture and until acknowledged", () => {
    expect(shouldShowCloudNudge("cloud", [])).toBe(true);
    expect(shouldShowCloudNudge("cloud", undefined)).toBe(true);
    expect(shouldShowCloudNudge("cloud", ["cloud_posture_default_changed"])).toBe(false);
    expect(shouldShowCloudNudge("local", [])).toBe(false);
  });
});

describe("shell banner text", () => {
  it("wraps to two lines instead of truncating", () => {
    const s = bannerTextStyle(1);
    expect(BANNER_MAX_LINES).toBe(2);
    expect(s.WebkitLineClamp).toBe(2);
    expect(s.whiteSpace).toBe("normal");
  });

  it("sizes in px times the width class's chrome scale", () => {
    expect(bannerTextStyle(gridMeasures("medium", false).scale).fontSize).toBe(BANNER_TEXT_PX);
    const tv = gridMeasures("tv", false).scale;
    expect(tv).toBeGreaterThan(1);
    expect(bannerTextStyle(tv).fontSize).toBeCloseTo(BANNER_TEXT_PX * tv);
  });
});
