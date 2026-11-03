import { describe, expect, it } from "vitest";

import { homeTabFor, tabScreens } from "@/nav/registry";
import type { GroundRole } from "@/shared/api/types";
import type { AgentProfile } from "@/shared/use-profile";

const ids = (profile: AgentProfile, role: GroundRole | null) => tabScreens(profile, role).map((s) => s.id);

describe("ground-station tabs by role", () => {
  it("a direct receiver has Link but no Mesh or Sources", () => {
    const tabs = ids("ground_station", "direct");
    expect(tabs).toContain("link");
    expect(tabs).not.toContain("mesh");
    expect(tabs).not.toContain("sources");
  });

  it("a relay adds Mesh only", () => {
    const tabs = ids("ground_station", "relay");
    expect(tabs).toContain("mesh");
    expect(tabs).not.toContain("sources");
  });

  it("a receiver adds Mesh and Sources", () => {
    const tabs = ids("ground_station", "receiver");
    expect(tabs).toContain("mesh");
    expect(tabs).toContain("sources");
  });

  it("a drone shows no ground-station tab", () => {
    const tabs = ids("drone", null);
    expect(tabs).not.toContain("mesh");
    expect(tabs).not.toContain("sources");
    expect(tabs).toContain("pair");
  });
});

describe("profile homes", () => {
  it("opens a drone and a ground station on the Feed and a node on Node", () => {
    expect(homeTabFor("drone")).toBe("feed");
    expect(homeTabFor("ground_station")).toBe("feed");
    expect(homeTabFor("workstation")).toBe("node");
    expect(homeTabFor("compute")).toBe("node");
  });
});
