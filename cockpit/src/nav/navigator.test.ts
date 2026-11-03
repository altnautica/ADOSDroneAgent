import { describe, expect, it } from "vitest";

import { DEFAULT_TAB_ID, homeTabId, tabVisibleFor } from "@/nav/navigator";

const MESH = { profiles: ["ground_station" as const], roles: ["relay" as const, "receiver" as const] };
const NODE = { profiles: ["workstation" as const, "compute" as const] };

describe("tab visibility", () => {
  it("shows every tab until the profile is known", () => {
    expect(tabVisibleFor(MESH, null)).toBe(true);
    expect(tabVisibleFor(NODE, "unknown")).toBe(true);
  });

  it("hides a tab on a profile it does not list", () => {
    expect(tabVisibleFor(MESH, "drone")).toBe(false);
    expect(tabVisibleFor(NODE, "workstation")).toBe(true);
    expect(tabVisibleFor(NODE, "ground_station")).toBe(false);
  });

  it("hides Mesh on a direct ground station", () => {
    expect(tabVisibleFor(MESH, "ground_station", "direct")).toBe(false);
    expect(tabVisibleFor(MESH, "ground_station", "relay")).toBe(true);
    expect(tabVisibleFor(MESH, "ground_station", "receiver")).toBe(true);
  });

  it("hides nothing by role while the role is unknown", () => {
    expect(tabVisibleFor(MESH, "ground_station", null)).toBe(true);
  });

  it("shows a tab with no gates everywhere", () => {
    expect(tabVisibleFor({}, "visor", null)).toBe(true);
  });
});

describe("profile home tabs", () => {
  const all = () => true;

  it("opens each profile on its own home", () => {
    expect(homeTabId("drone", all)).toBe("feed");
    expect(homeTabId("ground_station", all)).toBe("feed");
    expect(homeTabId("visor", all)).toBe("capture");
    expect(homeTabId("workstation", all)).toBe("node");
    expect(homeTabId("compute", all)).toBe("node");
  });

  it("has no home while the profile is unknown", () => {
    expect(homeTabId(null, all)).toBeNull();
    expect(homeTabId("unknown", all)).toBeNull();
  });

  it("falls back to the Feed for a home this build does not register", () => {
    expect(homeTabId("visor", (id) => id !== "capture")).toBe(DEFAULT_TAB_ID);
  });
});
