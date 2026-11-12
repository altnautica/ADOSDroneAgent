// Every free-text field on the panel kiosk opens the on-screen keyboard: the
// Params/MSP value entry and search boxes, and the settings text row.

import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const src = (file: string) => readFileSync(path.resolve(__dirname, "../..", file), "utf8");

describe("on-screen keyboard reach", () => {
  it("the value entry opens the keyboard on focus and commits when it closes", () => {
    const s = src("components/fc/fc-editors.tsx");
    expect(s).toContain("useWantsKeyboard()");
    expect(s).toMatch(/<KeyboardPanel[\s\S]*onClose=\{\(\) => \{\s*setKeyboard\(false\);\s*commit\(\);/);
    expect(s).toContain("if (!keyboard) commit();");
  });

  it("the Params and MSP searches use the keyboard-aware search box", () => {
    for (const f of ["components/screens/params-screen.tsx", "components/screens/msp-screen.tsx"]) {
      const s = src(f);
      expect(s, f).toContain("<SearchField");
      expect(s, f).not.toContain('type="search"');
    }
    expect(src("components/ui/search-field.tsx")).toContain("useWantsKeyboard()");
  });

  it("the settings text row shares the same rule", () => {
    expect(src("components/ui/setting-row.tsx")).toContain("useWantsKeyboard()");
  });
});
