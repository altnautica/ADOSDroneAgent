import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const read = (f: string) => readFileSync(path.resolve(__dirname, "..", f), "utf8");

describe("touch hit targets", () => {
  it("buttons grow to 44 px below md in every size", () => {
    const src = read("components/ui/button.tsx");
    for (const size of ["default", "sm", "lg"]) {
      expect(src).toMatch(new RegExp(`${size}: "[^"]*max-md:h-11`));
    }
    expect(src).toMatch(/icon: "h-9 w-9 max-md:h-11 max-md:w-11"/);
    expect(src).toMatch(/\$\{COARSE_HIT_AREA_SM\}/);
  });

  it("a switch keeps its drawn size and takes taps from a larger box on a touch screen", () => {
    const src = read("components/ui/switch.tsx");
    expect(src).toMatch(/h-5 w-9/);
    expect(src).toMatch(/COARSE_HIT_AREA,/);
    expect(read("lib/hit-area.ts")).toMatch(/\[@media\(pointer:coarse\)\]:after:-inset-3/);
  });

  it.each([
    ["components/panels/status-tiles.tsx", "summary pill"],
    ["components/panels/video-layers.tsx", "layer checkboxes"],
    ["components/telemetry/mavlink-params.tsx", "modified-only checkbox"],
    ["routes/telemetry-route.tsx", "telemetry tabs"],
    ["components/layout/sidebar.tsx", "nav links"],
    ["components/fc/bitmask-modal.tsx", "bit rows"],
  ])("%s: %s are 44 px tall below md", (file) => {
    expect(read(file)).toMatch(/max-md:min-h-11/);
  });
});
