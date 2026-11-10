import { readFileSync } from "node:fs";
import path from "node:path";

import { afterEach, describe, expect, it, vi } from "vitest";

import { currentArFrame, pilotFresh } from "@/hooks/use-pilot";
import { layerFrames } from "@/lib/layer-frames";
import { nextWaypointEtaS } from "@/shared/hud/chip-inputs";
import { parseArFrame } from "@/shared/hud/cockpit-frames";

const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const ar = parseArFrame(JSON.parse(readFileSync(path.join(FIXTURES, "ar-frame.json"), "utf8")))!;

const currentEta = () => {
  const { arFrame, arAgeMs } = currentArFrame();
  return nextWaypointEtaS(arFrame, arAgeMs);
};

describe("chip ETA clock", () => {
  afterEach(() => vi.restoreAllMocks());

  it("ages the ar frame on the monotonic clock, whatever the wall clock does", () => {
    layerFrames.putAr(ar);
    // The ground station sets its wall clock an hour forward after the page loaded.
    vi.spyOn(Date, "now").mockReturnValue(Date.now() + 3_600_000);
    expect(currentEta()).toBe(42);
    // And backwards: a frame 3 s old on the monotonic clock still expires.
    vi.spyOn(Date, "now").mockReturnValue(Date.now() - 3_600_000);
    layerFrames.ar.seen = performance.now() - 3000;
    expect(currentEta()).toBeNull();
  });
});

describe("pilot freshness", () => {
  const frame = { data: {} as never, at: 10_000 };
  it("is live up to the spec's stale_after_ms and stale after it", () => {
    expect(pilotFresh(frame, 10_000 + 2000)).toBe(true);
    expect(pilotFresh(frame, 10_000 + 2001)).toBe(false);
    expect(pilotFresh(null, 10_000)).toBe(false);
  });
});
