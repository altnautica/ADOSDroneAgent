import { describe, expect, it } from "vitest";

import { buildBanners } from "./banners";
import { gateDecision, groundRoleFrom } from "./node-identity";

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
});
