import { readFileSync } from "node:fs";
import path from "node:path";

import type { TFunction } from "i18next";
import { describe, expect, it } from "vitest";

import { exposureError, gainError } from "./exposure-draft";

const t = ((key: string) => key) as unknown as TFunction;
const read = (f: string) => readFileSync(path.resolve(__dirname, "..", f), "utf8");

describe("settings validation", () => {
  it("names a non-number exposure instead of disabling Apply in silence", () => {
    expect(exposureError("abc", t)).toBe("settings.shared.enterNumber");
    expect(exposureError("1.5", t)).toBe("settings.shared.enterWholeNumber");
    expect(exposureError("0", t)).toBe("settings.shared.mustBeAtLeast");
    expect(exposureError("5000000000", t)).toBe("settings.shared.mustBeAtMost");
    expect(exposureError("8000", t)).toBeNull();
  });

  it("bounds the gain at the agent's minimum", () => {
    expect(gainError("0", t)).toBe("settings.shared.mustBeAtLeast");
    expect(gainError("1.5", t)).toBeNull();
  });

  it("applies exposure only when dirty and shows the error inline", () => {
    const src = read("routes/settings/capture-settings.tsx");
    expect(src).toMatch(/disabled=\{busy \|\| !valid \|\| !dirty\}/);
    expect(src).toMatch(/\{expErr \?\? gainErr\}/);
  });

  it("bounds the MAVLink baud above as well as below", () => {
    expect(read("routes/settings/mavlink-settings.tsx")).toMatch(/max=\{MAX_BAUD\}/);
  });

  it("a FC transport change waits for a confirm, and arrows never select", () => {
    expect(read("routes/settings/mavlink-settings.tsx")).toMatch(/sourceConfirmTitle/);
    const group = read("components/ui/radio-card-group.tsx");
    expect(group).toMatch(/function focusIndex\(i: number\) \{\s*buttonsRef\.current\[i\]\?\.focus\(\);\s*\}/);
  });
});
