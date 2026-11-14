import { describe, expect, it } from "vitest";

import { BOTTOM_CHROME_CLEARANCE_PX } from "@/lib/cockpit-layout";
import {
  avoidChip,
  followButton,
  RADAR_PX,
  railBottomPx,
  railButtonPx,
  railLayout,
  reacquiredToastText,
  SETTING_ECHO_TIMEOUT_MS,
  settingDisplay,
} from "@/lib/follow-rail";

const rail = { followInstalled: true, hasPattern: false, hasModeSetting: true, canSwap: true, safeHeightPx: 600 };

describe("railLayout", () => {
  it("shows the follow group and the camera buttons at every class", () => {
    for (const width of ["compact", "medium", "expanded", "tv"] as const) {
      expect(railLayout({ ...rail, width })).toEqual({ buttons: ["follow", "mode", "camSwap", "rec"], folded: [] });
    }
  });

  it("never repeats the left column's hold, return home or land", () => {
    for (const width of ["compact", "medium", "expanded", "tv"] as const) {
      const b = railLayout({ ...rail, width }).buttons as string[];
      expect(b.some((x) => x === "hold" || x === "rth" || x === "land")).toBe(false);
    }
  });

  it("folds 3–4 on a short compact panel", () => {
    expect(railLayout({ ...rail, width: "compact", safeHeightPx: 359 })).toMatchObject({
      buttons: ["follow", "mode"],
      folded: ["camSwap", "rec"],
    });
    expect(railLayout({ ...rail, width: "medium", safeHeightPx: 200 }).folded).toEqual([]);
  });

  it("drops what is not available", () => {
    expect(railLayout({ ...rail, width: "medium", followInstalled: false, canSwap: false }).buttons).toEqual(["rec"]);
    expect(railLayout({ ...rail, width: "medium", hasModeSetting: false }).buttons).toEqual(["follow", "camSwap", "rec"]);
  });
});

describe("rail sizes", () => {
  it("sizes buttons per class and for gloves", () => {
    expect([railButtonPx("compact", false), railButtonPx("medium", false), railButtonPx("expanded", false), railButtonPx("tv", false)]).toEqual([48, 48, 56, 64]);
    expect(railButtonPx("compact", true)).toBe(60);
  });

  it("keeps clear of a corner radar at compact and medium only", () => {
    expect(railBottomPx("compact", true)).toBe(BOTTOM_CHROME_CLEARANCE_PX + RADAR_PX.compact + 8);
    expect(railBottomPx("medium", false)).toBe(BOTTOM_CHROME_CLEARANCE_PX);
    expect(railBottomPx("tv", true)).toBe(BOTTOM_CHROME_CLEARANCE_PX);
  });
});

describe("followButton", () => {
  it("reads FOLLOW when idle with a target and TRACKING while active", () => {
    expect(followButton(false, "idle", true)).toBe("follow");
    expect(followButton(false, null, false)).toBeNull();
    expect(followButton(true, "idle", false)).toBe("tracking");
    expect(followButton(false, "coasting", false)).toBe("tracking");
  });
});

describe("settingDisplay", () => {
  const base = { echo: "smart", fallback: "smart", requested: null, requestedAt: null, now: 10_000 };

  it("shows the echo, else the default", () => {
    expect(settingDisplay(base)).toEqual({ value: "smart", pending: false, expired: false, confirmed: false });
    expect(settingDisplay({ ...base, echo: null, fallback: "strict" }).value).toBe("strict");
  });

  it("shows a write as pending until echoed", () => {
    const req = { ...base, requested: "strict", requestedAt: 9000 };
    expect(settingDisplay(req)).toEqual({ value: "strict", pending: true, expired: false, confirmed: false });
    expect(settingDisplay({ ...req, echo: "strict" })).toEqual({ value: "strict", pending: false, expired: false, confirmed: true });
  });

  it("reverts when no echo came within 3 s", () => {
    const req = { ...base, requested: "strict", requestedAt: 10_000 - SETTING_ECHO_TIMEOUT_MS - 1 };
    expect(settingDisplay(req)).toEqual({ value: "smart", pending: false, expired: true, confirmed: false });
  });
});

describe("avoidChip", () => {
  it("maps the pilot's avoidance state", () => {
    expect(avoidChip("detour")).toEqual({ text: "AVOIDING", tone: "warn" });
    expect(avoidChip("blocked")?.tone).toBe("crit");
    expect(avoidChip("stale")).toEqual({ text: "OBSTACLE DATA STALE", tone: "warn" });
    expect(avoidChip("unavailable")).toEqual({ text: "NO OBSTACLE SENSOR", tone: "warn" });
    expect(avoidChip(null)).toBeNull();
  });
});

describe("reacquiredToastText", () => {
  it("matches the T12 vector", () => {
    expect(reacquiredToastText({ topic: "follow.reacquired", trackId: 15, similarity: 0.862, gapS: 1.8 })).toBe(
      "Re-acquired #15 · 0.86 · 1.8 s gap",
    );
    expect(reacquiredToastText({ topic: "follow.lost", trackId: 15, reason: "timeout" })).toBeNull();
  });
});
