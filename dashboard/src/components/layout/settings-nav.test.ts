import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const src = readFileSync(path.resolve(__dirname, "settings-layout.tsx"), "utf8");

describe("settings nav on a phone and from the keyboard", () => {
  it("collapses to a section select below md and hides the link list there", () => {
    expect(src).toMatch(/<label className="mb-4 block md:hidden">/);
    expect(src).toMatch(/onChange=\{\(e\) => navigate\(e\.target\.value\)\}/);
    expect(src).toMatch(/<nav className="hidden md:block space-y-4">/);
  });

  it("offers a skip link to the section content", () => {
    expect(src).toMatch(/href="#settings-content"/);
    expect(src).toMatch(/<main id="settings-content" tabIndex=\{-1\}/);
  });
});
