import { describe, expect, it } from "vitest";

import { armLabel, batteryLabel, linkLabel, reachBadge, videoLabel } from "@/lib/safety-band";
import type { GsStatus } from "@/lib/types";

describe("safety band mapping", () => {
  it("names the reach", () => {
    expect(reachBadge("drone", "localhost")).toBe("DIRECT");
    expect(reachBadge("drone", "192.168.1.50")).toBe("LAN");
    expect(reachBadge("ground_station", "localhost")).toBe("VIA GROUND");
    expect(reachBadge(null, "localhost")).toBeNull();
  });

  it("shows a dash for every unknown value", () => {
    expect(armLabel(false, true)).toBe("—");
    expect(linkLabel(null)).toBe("—");
    expect(batteryLabel(null, { battery: { voltage: 0, current: null, remaining: 0, temperature: null } })).toBe("—");
  });

  it("formats known values", () => {
    expect(armLabel(true, true)).toBe("ARMED");
    expect(linkLabel({ heartbeat_age_s: 0.42 } as GsStatus)).toBe("HB 0.4s");
    expect(
      batteryLabel(
        { enabled: true, stale: false, updated_at_ms: 1, packs: [{ id: 0, stale: false, remaining_pct: 62, prediction: { eta_s: 240 } }] },
        null,
      ),
    ).toBe("62% 4:00");
    expect(videoLabel({ state: "failed", transport: null, highLatency: false, error: null, width: null, height: null })).toBe(
      "NO VIDEO",
    );
  });
});
