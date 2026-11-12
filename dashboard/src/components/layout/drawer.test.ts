import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const read = (f: string) => readFileSync(path.resolve(__dirname, f), "utf8");

describe("mobile nav drawer", () => {
  it("is a modal dialog: focus trap, Escape and a title come from Radix Dialog", () => {
    const src = read("mobile-nav-drawer.tsx");
    expect(src).toMatch(/<DialogPrimitive\.Root open=\{mobileNavOpen\}/);
    expect(src).toMatch(/<DialogPrimitive\.Content/);
    expect(src).toMatch(/<DialogPrimitive\.Title className="sr-only">/);
  });

  it("loads on the first open, so the dialog stack stays out of the main chunk", () => {
    const src = read("sidebar.tsx");
    expect(src).not.toMatch(/@radix-ui\/react-dialog/);
    expect(src).toMatch(/lazy\(\(\) =>\s*import\("@\/components\/layout\/mobile-nav-drawer"\)/);
  });
});
