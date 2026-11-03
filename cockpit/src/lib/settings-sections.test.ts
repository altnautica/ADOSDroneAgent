import { describe, expect, it } from "vitest";

import {
  SETTINGS_SECTIONS,
  SETTINGS_SECTION_IDS,
  isSettingsSectionId,
  sectionVisibleFor,
  visibleSections,
} from "./settings-sections";

const ids = (p: Parameters<typeof visibleSections>[0], r: Parameters<typeof visibleSections>[1] = null) =>
  visibleSections(p, r).map((s) => s.id);

describe("settings sections", () => {
  it("catalogues every id exactly once", () => {
    expect(SETTINGS_SECTIONS.map((s) => s.id).sort()).toEqual([...SETTINGS_SECTION_IDS].sort());
  });

  it("recognises section ids but not panel pages", () => {
    expect(isSettingsSectionId("wifi")).toBe(true);
    expect(isSettingsSectionId("appearance")).toBe(false);
    expect(isSettingsSectionId("gamepad")).toBe(false);
  });

  it("gates by profile", () => {
    const drone = ids("drone");
    expect(drone).toContain("mavlink");
    expect(drone).toContain("swarm");
    expect(drone).not.toContain("uplink");
    expect(drone).not.toContain("capture");

    const gs = ids("ground_station", "direct");
    expect(gs).toEqual(expect.arrayContaining(["uplink", "display", "region"]));
    expect(gs).not.toContain("battery");

    expect(ids("visor")).toEqual(expect.arrayContaining(["capture", "battery", "sensors"]));
    expect(ids("visor")).not.toContain("region");
  });

  it("shows only ungated sections while the profile is unknown", () => {
    for (const p of [null, "unknown"] as const) {
      const visible = ids(p);
      expect(visible).toContain("security");
      expect(visible).not.toContain("mavlink");
    }
  });

  it("passes a role gate while the role is unknown and applies it once known", () => {
    const spec = { id: "uplink" as const, profiles: ["ground_station"] as const, roles: ["direct"] as const };
    expect(sectionVisibleFor(spec, "ground_station", null)).toBe(true);
    expect(sectionVisibleFor(spec, "ground_station", "direct")).toBe(true);
    expect(sectionVisibleFor(spec, "ground_station", "relay")).toBe(false);
  });
});
