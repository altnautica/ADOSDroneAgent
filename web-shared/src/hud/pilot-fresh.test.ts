import { describe, expect, it } from "vitest";

import flightStateJson from "../cockpit-spec/flight-state.json";
import { pilotFresh } from "./pilot-fresh";

describe("pilotFresh", () => {
  it("is fresh below the spec's stale age and stale from it on", () => {
    expect(flightStateJson.stale_after_ms).toBe(2000);
    expect(pilotFresh(1000, 1000)).toBe(true);
    expect(pilotFresh(1000, 2999)).toBe(true);
    expect(pilotFresh(1000, 3000)).toBe(false);
    expect(pilotFresh(0, 10_000)).toBe(false);
  });

  it("is never fresh without a frame", () => {
    expect(pilotFresh(null, 0)).toBe(false);
    expect(pilotFresh(undefined, 0)).toBe(false);
  });
});
