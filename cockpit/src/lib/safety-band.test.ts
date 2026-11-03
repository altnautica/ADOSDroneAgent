import { describe, expect, it } from "vitest";

import { armLabel, batteryLabel, linkLabel, preflightChecks, reachBadge, videoLabel } from "@/lib/safety-band";
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

  it("evaluates preflight from real readings only", () => {
    const checks = preflightChecks({ telemetry: null, battery: null, live: false, home: null });
    expect(checks.every((c) => !c.ok)).toBe(true);
    const ok = preflightChecks({
      telemetry: {
        gps: { fix_type: 3, satellites: 12, eph: null, epv: null },
        battery: { voltage: 16, current: null, remaining: 80, temperature: null },
      },
      battery: null,
      live: true,
      home: { lat: 1, lon: 2 },
    });
    expect(ok.map((c) => c.ok)).toEqual([true, true, true, true]);
  });
});
