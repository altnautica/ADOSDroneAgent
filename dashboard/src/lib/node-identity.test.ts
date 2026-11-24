import { describe, expect, it } from "vitest";

import { buildBanners } from "@/shared/api/banners";
import { gateDecision, groundRoleFrom, meshReadEnabled } from "./node-identity";

describe("gateDecision", () => {
  it("passes through only while the profile is not answered yet (auto)", () => {
    expect(gateDecision("auto", "direct", ["drone"])).toBe("pass");
  });

  it("treats an unrecognised profile as unavailable", () => {
    expect(gateDecision("unknown", "direct", ["drone"])).toBe("unavailable");
    expect(gateDecision("unknown", "direct", ["drone", "ground_station"])).toBe("unavailable");
  });

  it("gates on profile and ground role", () => {
    expect(gateDecision("drone", "direct", ["drone"])).toBe("pass");
    expect(gateDecision("drone", "direct", ["ground_station"])).toBe("mismatch");
    expect(gateDecision("ground_station", "direct", ["ground_station"], ["receiver"])).toBe("mismatch");
    expect(gateDecision("ground_station", "receiver", ["ground_station"], ["receiver"])).toBe("pass");
  });

  it("maps the wire role, defaulting to direct", () => {
    expect(groundRoleFrom("relay")).toBe("relay");
    expect(groundRoleFrom(null)).toBe("direct");
    expect(groundRoleFrom("bogus")).toBe("direct");
  });
});

describe("buildBanners", () => {
  it("fires the FC banner on a resolved drone profile", () => {
    const ids = buildBanners({
      agentUnreachable: false,
      profile: "drone",
      fcConnected: false,
      fcReachable: false,
    }).map((b) => b.id);
    expect(ids).toEqual(["fc-disconnected"]);
  });

  it("stays quiet for an MSP FC that is reachable, or with no snapshot read", () => {
    expect(
      buildBanners({ agentUnreachable: false, profile: "drone", fcConnected: false, fcReachable: true }),
    ).toEqual([]);
    expect(
      buildBanners({ agentUnreachable: false, profile: "drone", fcConnected: null, fcReachable: false }),
    ).toEqual([]);
  });

  it("never fires the FC banner off a drone", () => {
    expect(
      buildBanners({ agentUnreachable: false, profile: "ground_station", fcConnected: false, fcReachable: false }),
    ).toEqual([]);
  });

  it("says a ground station is re-pairing a drone on the air with a different key", () => {
    const base = { agentUnreachable: false, fcConnected: null, fcReachable: null };
    const misKeyed = { cause: "mis_keyed", active: true, at: 700, episodes: 1 };
    expect(buildBanners({ ...base, profile: "ground_station", pairRearm: misKeyed })).toEqual([
      { id: "gs-repairing", severity: "warn", message: "Ground station is re-pairing: its drone is on the air with a different key." },
    ]);
    expect(buildBanners({ ...base, profile: "ground_station", pairRearm: { ...misKeyed, active: false } })).toEqual([]);
    expect(buildBanners({ ...base, profile: "ground_station", pairRearm: { ...misKeyed, cause: "unproven" } })).toEqual([]);
    expect(buildBanners({ ...base, profile: "drone", pairRearm: misKeyed })).toEqual([]);
  });

  it("says a drone's delivered fleet slot applies after landing, and when the restart onto it fails", () => {
    const base = { agentUnreachable: false, fcConnected: null, fcReachable: null };
    const fleet = (outcome: string) => ({ outcome, fleetId: 7, fleetSlot: 3 });
    expect(buildBanners({ ...base, profile: "drone", fleetApply: fleet("deferred_armed") })).toEqual([
      { id: "fleet-slot-pending", severity: "warn", message: "New fleet slot 3 applies after landing." },
    ]);
    expect(buildBanners({ ...base, profile: "drone", fleetApply: fleet("restart_failed") })).toEqual([
      { id: "fleet-slot-failed", severity: "err", message: "Radio restart onto fleet slot 3 failed. Retrying." },
    ]);
    expect(buildBanners({ ...base, profile: "drone", fleetApply: fleet("applied") })).toEqual([]);
    expect(buildBanners({ ...base, profile: "ground_station", fleetApply: fleet("deferred_armed") })).toEqual([]);
  });
});

describe("meshReadEnabled", () => {
  it("does not read the mesh route on a direct ground station", () => {
    expect(meshReadEnabled(groundRoleFrom("direct"))).toBe(false);
    expect(meshReadEnabled(groundRoleFrom(null))).toBe(false);
    expect(meshReadEnabled("relay")).toBe(true);
    expect(meshReadEnabled("receiver")).toBe(true);
  });
});
