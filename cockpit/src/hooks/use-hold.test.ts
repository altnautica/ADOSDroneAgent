import { describe, expect, it } from "vitest";

import { KEEP_HOLDING_HINT_MS, claimsPanelHold, keepHoldingText } from "@/hooks/use-hold";
import { STOP_HOLD_MS } from "@/lib/pad/ds4-map";

describe("the keep-holding hint", () => {
  it("names the seconds the hold still lacked, from the spec text", () => {
    expect(keepHoldingText(1500)).toBe("Keep holding · 1.5 s");
    expect(keepHoldingText(420)).toBe("Keep holding · 0.4 s");
    expect(keepHoldingText(-5)).toBe("Keep holding · 0.0 s");
  });

  it("stays 1.5 s", () => {
    expect(KEEP_HOLDING_HINT_MS).toBe(1500);
  });
});

describe("a panel long-press on a focused hold", () => {
  it("completes a hold up to the long press, never the stop hold", () => {
    expect(claimsPanelHold(1500)).toBe(true);
    expect(claimsPanelHold(2000)).toBe(true);
    expect(claimsPanelHold(STOP_HOLD_MS)).toBe(false);
  });
});
