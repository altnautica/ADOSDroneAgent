import { describe, expect, it } from "vitest";

import { stepLeafChange } from "@/lib/settings/guarded-leaf";
import { FORMATIONS, FORMATION_LABEL, toFormation } from "@/lib/settings/swarm";

describe("swarm formations", () => {
  it("offers exactly the built-in generators, each labelled", () => {
    expect(FORMATIONS).toEqual(["line", "column", "wedge", "grid", "circle"]);
    for (const f of FORMATIONS) expect(FORMATION_LABEL[f].labelKey).toMatch(/^settingsMore\.swarm\./);
  });

  it("never reads an unknown formation as a valid one", () => {
    expect(toFormation("wedge")).toBe("wedge");
    expect(toFormation("diamond")).toBeNull();
    expect(toFormation(undefined)).toBeNull();
  });

  it("writes without a guard", () => {
    expect(stepLeafChange({ key: "swarm.enabled", value: true }, false).kind).toBe("write");
  });
});
