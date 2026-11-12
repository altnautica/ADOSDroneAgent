import { describe, expect, it } from "vitest";

import { freshNavState, NAV_STATE_FRESH_MS } from "@/hooks/use-nav-source";
import type { NavState } from "@/shared/hud/nav-types";

describe("vision-nav's report as a nav-source input", () => {
  const data = { source: "vio" } as unknown as NavState;

  it("counts under 2000 ms and not from 2000 ms", () => {
    expect(NAV_STATE_FRESH_MS).toBe(2000);
    expect(freshNavState({ data, at: 1000 }, 1000 + 1999)).toBe(data);
    expect(freshNavState({ data, at: 1000 }, 1000 + 2000)).toBeNull();
    expect(freshNavState(null, 1000)).toBeNull();
  });
});
