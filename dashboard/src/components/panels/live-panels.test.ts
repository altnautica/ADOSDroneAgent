import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const read = (f: string) => readFileSync(path.resolve(__dirname, "../..", f), "utf8");

describe("agent down: panels never show the last read as current", () => {
  it.each([
    ["routes/diagnostics-route.tsx", /const sysData = liveData\(sys, /, /sys\.data|services\.data/],
    ["components/panels/mesh-panel.tsx", /const m = liveData\(mesh, /, /mesh\.data/],
    ["components/panels/sources-panel.tsx", /const c = liveData\(combined, /, /combined\.data|relays\.data/],
    ["components/panels/capture-panel.tsx", /const live = liveData\(capture, /, /capture\.data/],
  ])("%s", (file, live, stale) => {
    const src = read(file);
    expect(src).toMatch(live);
    expect(src).not.toMatch(stale);
  });

  it("the mesh role and the header profile read as words", () => {
    expect(read("components/panels/mesh-panel.tsx")).toMatch(/\{roleLabel\(role\)\}/);
    expect(read("components/layout/header.tsx")).toMatch(/\{profileLabel\(profile\)\}/);
  });
});
