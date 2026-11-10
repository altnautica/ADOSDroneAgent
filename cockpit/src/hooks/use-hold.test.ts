import { describe, expect, it } from "vitest";

import { KEEP_HOLDING_HINT_MS, keepHoldingText } from "@/hooks/use-hold";

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
