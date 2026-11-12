import { describe, expect, it } from "vitest";

import { fcHomeSet, preflightChecks, preflightFailing } from "./preflight";

const ground = {
  armed: false,
  gps: { fix_type: 3 },
  battery: { voltage: 16, current: null, remaining: 80, temperature: null },
};

function homeOk(telemetry: Parameters<typeof preflightChecks>[0]["telemetry"]): boolean | undefined {
  return preflightChecks({ telemetry, battery: null, live: true }).find((c) => c.id === "home")?.ok;
}

describe("preflight checks", () => {
  it("evaluates from real readings only", () => {
    const checks = preflightChecks({ telemetry: null, battery: null, live: false });
    expect(checks.map((c) => c.id)).toEqual(["link", "gps", "battery", "home"]);
    expect(checks.every((c) => !c.ok)).toBe(true);
  });

  it("passes home on the ground once the FC reports HOME_POSITION", () => {
    expect(homeOk({ ...ground, home: { lat_e7: 125000000, lon_e7: 775000000 } })).toBe(true);
    expect(preflightChecks({ telemetry: { ...ground, home: { lat_e7: 1, lon_e7: 2 } }, battery: null, live: true }).every((c) => c.ok)).toBe(true);
  });

  it("passes home on an EKF origin without GNSS", () => {
    expect(homeOk({ armed: false, home: { lat_e7: null, lon_e7: null, local_ned: [0, 0, 0] } })).toBe(true);
    expect(homeOk({ armed: false, ekf: { flags: 16 | 2 } })).toBe(true);
  });

  it("fails home while the FC has neither a home nor an origin", () => {
    expect(homeOk(ground)).toBe(false);
    expect(homeOk({ ...ground, home: null })).toBe(false);
    expect(homeOk({ ...ground, home: { lat_e7: 0, lon_e7: 0 } })).toBe(false);
    expect(homeOk({ ...ground, ekf: { flags: 8 | 2 } })).toBe(false);
    expect(fcHomeSet(null)).toBe(false);
  });

  it("lists no home item on the ground once the FC has a home", () => {
    const checks = preflightChecks({ telemetry: { ...ground, home: { lat_e7: 1, lon_e7: 2 } }, battery: null, live: true });
    expect(preflightFailing(checks, [], false)).toEqual([]);
    const without = preflightChecks({ telemetry: ground, battery: null, live: true });
    expect(preflightFailing(without, [], false)).toEqual([{ id: "home", label: "Home position not set" }]);
    expect(preflightFailing(without, [], true)).toEqual([]);
    expect(preflightFailing(without, [], undefined)).toEqual([]);
    expect(preflightFailing(without, [], null)).toEqual([]);
  });
});
