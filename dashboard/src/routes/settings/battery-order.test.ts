import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const src = readFileSync(path.resolve(__dirname, "battery-settings.tsx"), "utf8");

describe("battery thresholds", () => {
  it("keeps critical below low before Save", () => {
    expect(src).toMatch(/Math\.max\(f\.min, battery\.critical_cell_mv \+ 1\)/);
    expect(src).toMatch(/Math\.min\(f\.max, battery\.low_cell_mv - 1\)/);
    expect(src).toMatch(/settingsMore\.battery\.orderNote/);
  });
});
