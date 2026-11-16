// The pad's feedback driver: a stale pilot frame fed again (the HTTP
// fallback repeats the last frame every poll) never re-arms the lost-state
// rumble, so a lost link rumbles critical once.

import { describe, expect, it } from "vitest";

import { PilotFeedback, type RumblePattern } from "@/lib/pad/rumble";
import { CHIP_STALE_AFTER_MS } from "@/shared/hud/flight-state";

const FRAME = { owner: "pilot", profile: "normal", armed: true, input_session: { src: "gs", session: 7, label: "GS" } };

describe("pad feedback over a stale pilot frame", () => {
  it("rumbles critical once while the same stale frame keeps arriving", () => {
    const played: RumblePattern[] = [];
    const fb = new PilotFeedback((p) => played.push(p));
    const at = 1_000;
    fb.frame(FRAME, at, at);
    // The link goes quiet: the frame ages past the stale threshold.
    let now = at + CHIP_STALE_AFTER_MS + 1;
    fb.tick(at, now);
    expect(played).toEqual(["critical"]);
    // The fallback poll hands back the same old frame every 500 ms.
    for (let i = 0; i < 6; i++) {
      now += 500;
      fb.frame({ ...FRAME }, at, now);
      fb.tick(at, now);
    }
    expect(played).toEqual(["critical"]);
  });

  it("takes a fresh frame after the loss as a first state", () => {
    const played: RumblePattern[] = [];
    const fb = new PilotFeedback((p) => played.push(p));
    fb.frame(FRAME, 1_000, 1_000);
    fb.tick(1_000, 1_000 + CHIP_STALE_AFTER_MS + 1);
    const back = 10_000;
    fb.frame({ ...FRAME, profile: "sport" }, back, back);
    expect(played).toEqual(["critical"]);
    fb.frame({ ...FRAME, profile: "cine" }, back + 20, back + 20);
    expect(played).toEqual(["critical", "mode_change"]);
  });
});
