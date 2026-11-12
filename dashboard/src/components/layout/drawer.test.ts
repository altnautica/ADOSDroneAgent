import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const src = readFileSync(path.resolve(__dirname, "sidebar.tsx"), "utf8");

describe("mobile nav drawer", () => {
  it("is a modal dialog: focus trap, Escape and a title come from Radix Dialog", () => {
    expect(src).toMatch(/<DialogPrimitive\.Root open=\{mobileNavOpen\}/);
    expect(src).toMatch(/<DialogPrimitive\.Content/);
    expect(src).toMatch(/<DialogPrimitive\.Title className="sr-only">/);
    expect(src).not.toMatch(/\{mobileNavOpen && \(/);
  });
});
