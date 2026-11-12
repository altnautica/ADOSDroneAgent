import type { TFunction } from "i18next";
import { describe, expect, it } from "vitest";

import { wfbStale, wfbStateBadge } from "@/lib/wfb-state";

const t = ((k: string) => k) as unknown as TFunction;

describe("WFB state badge", () => {
  it("treats stale as a known warning state with dimmed values", () => {
    expect(wfbStateBadge("stale", t)).toEqual({ label: "wfb.state.stale", variant: "warning" });
    expect(wfbStale("stale")).toBe(true);
    expect(wfbStale("active")).toBe(false);
  });

  it("claims nothing for an unknown state (agent down)", () => {
    expect(wfbStateBadge("unknown", t)).toBeNull();
  });
});
