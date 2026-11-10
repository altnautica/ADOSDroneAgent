import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import { pressOrExplain } from "@/lib/zone-press";
import { onToast, type ToastMessage } from "@/shared/api/toast";

const src = (p: string) => readFileSync(path.resolve(__dirname, p), "utf8");
const ZONES = ["../components/feed/cockpit/cockpit-zones.tsx", "../components/feed/feed-layouts.tsx"];

describe("an unavailable flight control explains itself on press", () => {
  it("shows the reason in the cockpit toast and does not run", () => {
    const seen: ToastMessage[] = [];
    const off = onToast((t) => seen.push(t));
    let ran = 0;
    pressOrExplain("On the ground", () => (ran += 1));
    pressOrExplain(null, () => (ran += 1));
    off();
    expect(ran).toBe(1);
    expect(seen.map((t) => t.message)).toEqual(["On the ground"]);
  });

  it("the column controls use aria-disabled, never the native disabled attribute", () => {
    for (const f of ZONES) {
      const s = src(f);
      expect(s, f).not.toMatch(/\sdisabled=\{/);
      expect(s, f).not.toMatch(/[\s"](?<!aria-)disabled:opacity/);
    }
    expect(src(ZONES[0])).toMatch(/aria-disabled=\{/);
  });

  it("draws no hard-coded flight-screen words in the zones (all via specText)", () => {
    for (const f of ZONES) {
      const s = src(f);
      expect(s, f).not.toMatch(/(aria-label|label|title)="[A-Z][a-z]/);
      expect(s, f).not.toMatch(/(aria-label|label|title)=\{`[A-Z][a-z]/);
      expect(s, f).not.toMatch(/: "(Map|Attitude|Obstacles)"/);
    }
  });

  it("the follow controls, follow range and on-screen sticks take their words from the spec", () => {
    const files = [
      "../components/feed/follow-rail.tsx",
      "../components/feed/follow-range-panel.tsx",
      "../components/feed/follow-chrome.tsx",
      "../components/feed/onscreen-sticks.tsx",
      "./follow-panel.ts",
      "../components/feed/pip-inset.tsx",
    ];
    for (const f of files) {
      const s = src(f);
      expect(s, f).not.toMatch(/"(Follow range|Follow controls|Follow target|Stop tracking|More controls|Left on-screen stick|Right on-screen stick)"/);
      expect(s, f).not.toMatch(/"(FOLLOW|REC|MORE|STALE|IDLE|HOLD|LOST|GND|BOX|LIDAR|SMART|STRICT)"/);
      expect(s, f).not.toMatch(/`(DEP|BRG|LAT) /);
      expect(s, f).not.toMatch(/"Recording did not (start|stop)"/);
      expect(s, f).not.toMatch(/TRACKING ×|Swap cameras|Swap \$\{/);
    }
  });

  it("the map view's video card swaps video and map with the spec label", () => {
    expect(src(ZONES[1])).toMatch(/specText\("cockpit\.pip\.swap"\)/);
  });
});
