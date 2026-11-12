import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

import { SPACING_BOUNDS } from "@/lib/settings/swarm";

describe("swarm settings", () => {
  it("refuses a zero spacing", () => {
    expect(SPACING_BOUNDS.min).toBeGreaterThan(0);
  });

  it("asks before turning the swarm runtime on and drops the config-only claim", () => {
    const src = readFileSync(path.resolve(__dirname, "../../components/settings/sections/swarm-section.tsx"), "utf8");
    expect(src).toContain("SlideConfirmDialog");
    expect(src).not.toContain("configOnlyBadge");
  });
});
