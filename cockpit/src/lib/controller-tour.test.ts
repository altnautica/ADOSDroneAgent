import { describe, expect, it } from "vitest";

import { TOUR_CARDS, tourAirborne, tourCapturesPad, tourCardRect, tourPadCommand, tourStage, tourStep } from "@/lib/controller-tour";

function overlaps(a: { x: number; y: number; w: number; h: number }, b: { x: number; y: number; w: number; h: number }): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

describe("first-connect controller tour", () => {
  it("opens on the first controller, once", () => {
    expect(tourStage(true, true, false, false)).toBe("tour");
    expect(tourStage(true, true, true, false)).toBe("none");
  });

  it("asks for a button press while the controller is announced but not reported", () => {
    expect(tourStage(false, true, false, false)).toBe("press");
    expect(tourStage(false, false, false, false)).toBe("none");
  });

  it("is airborne only when armed and not landed, else by the heartbeat", () => {
    expect(tourAirborne({ armed: true, landed: false }, null)).toBe(true);
    // Armed on the ground, or disarmed: on the ground.
    expect(tourAirborne({ armed: true, landed: true }, true)).toBe(false);
    expect(tourAirborne({ armed: false, landed: false }, true)).toBe(false);
    // No live pilot state: the heartbeat's arm state decides.
    expect(tourAirborne(null, true)).toBe(true);
    expect(tourAirborne(null, false)).toBe(false);
    expect(tourAirborne(null, null)).toBe(false);
  });

  it("waits while airborne on every screen, not only the flight screen", () => {
    const airborne = tourAirborne({ armed: true, landed: false }, null);
    expect(tourStage(true, true, false, airborne)).toBe("none");
    expect(tourStage(false, true, false, airborne)).toBe("none");
    expect(tourStage(true, true, false, tourAirborne({ armed: false, landed: true }, null))).toBe("tour");
  });

  it("maps the D-pad, Cross and Circle", () => {
    expect(tourPadCommand(0)).toBe("activate");
    expect(tourPadCommand(1)).toBe("close");
    expect(tourPadCommand(12)).toBe("prev");
    expect(tourPadCommand(14)).toBe("prev");
    expect(tourPadCommand(13)).toBe("next");
    expect(tourPadCommand(15)).toBe("next");
    expect(tourPadCommand(3)).toBeNull();
  });

  it("walks the cards with Cross and finishes on the last", () => {
    let c = { card: 0, focus: 0 };
    for (let i = 1; i < TOUR_CARDS.length; i += 1) {
      const r = tourStep(c, "activate");
      expect(r).toEqual({ card: i, focus: 0, done: false });
      c = r;
    }
    expect(tourStep(c, "activate").done).toBe(true);
  });

  it("skips from the focused Skip button, the Skip tap and Circle", () => {
    const onSkip = tourStep({ card: 0, focus: 0 }, "next");
    expect(onSkip).toEqual({ card: 0, focus: 1, done: false });
    expect(tourStep(onSkip, "activate").done).toBe(true);
    expect(tourStep({ card: 1, focus: 0 }, "skip").done).toBe(true);
    expect(tourStep({ card: 1, focus: 0 }, "close").done).toBe(true);
    // The focus wraps.
    expect(tourStep({ card: 0, focus: 0 }, "prev").focus).toBe(1);
  });

  it("places the card clear of the left column and STOP", () => {
    const leftColumn = { x: 8, y: 60, w: 72, h: 500 };
    const stop = { x: 1180, y: 8, w: 84, h: 40 };
    const r = tourCardRect(1280, 800, { leftColumn, stop, topBar: null, alertStack: null }, 360, 160, 120);
    expect(r.w).toBe(360);
    expect(overlaps(r, leftColumn)).toBe(false);
    expect(overlaps(r, stop)).toBe(false);
    expect(r.y + r.h).toBe(800 - 120);
    // A narrow window shrinks the card rather than cover the column.
    const narrow = tourCardRect(360, 640, { leftColumn: { x: 0, y: 0, w: 100, h: 640 }, stop: null, topBar: null, alertStack: null }, 360, 200, 120);
    expect(narrow.x).toBeGreaterThanOrEqual(100);
    expect(narrow.x + narrow.w).toBeLessThanOrEqual(360);
    // A short window pushes the card below STOP.
    const short = tourCardRect(800, 200, { leftColumn: null, stop: { x: 0, y: 0, w: 800, h: 40 }, topBar: null, alertStack: null }, 360, 100, 120);
    expect(overlaps(short, { x: 0, y: 0, w: 800, h: 40 })).toBe(false);
  });

  it("at a compact 580x360 stays below the top bar and the alert stack, shrinking to fit", () => {
    const topBar = { x: 0, y: 0, w: 580, h: 44 };
    const alertStack = { x: 140, y: 48, w: 300, h: 60 };
    const stop = { x: 500, y: 6, w: 72, h: 32 };
    const avoid = { leftColumn: { x: 8, y: 52, w: 64, h: 300 }, stop, topBar, alertStack };
    const r = tourCardRect(580, 360, avoid, 360, 200, 120);
    for (const zone of [topBar, alertStack, stop, avoid.leftColumn]) expect(overlaps(r, zone)).toBe(false);
    expect(r.y).toBeGreaterThanOrEqual(alertStack.y + alertStack.h + 12);
    expect(r.h).toBe(200);
    // A taller card shrinks (its body scrolls) rather than run off the bottom.
    const tall = tourCardRect(580, 360, avoid, 360, 260, 120);
    expect(tall.y).toBeGreaterThanOrEqual(alertStack.y + alertStack.h + 12);
    expect(tall.y + tall.h).toBeLessThanOrEqual(360 - 12);
    expect(tall.h).toBeLessThan(260);
    // With no alert up the card still stays clear of the top bar.
    const clear = tourCardRect(580, 360, { ...avoid, alertStack: null }, 360, 300, 120);
    expect(clear.y).toBeGreaterThanOrEqual(topBar.h + 12);
    expect(overlaps(clear, topBar)).toBe(false);
  });
});

describe("the tour's hold on the pad", () => {
  it("captures only when the vehicle is known to be on the ground and disarmed", () => {
    expect(tourCapturesPad({ armed: false, landed: true }, { live: false, armed: null })).toBe(true);
    expect(tourCapturesPad(null, { live: true, armed: false })).toBe(true);
  });

  it("leaves the pad to the safety bindings while the arm state is unknown or airborne", () => {
    // No fresh pilot and no live telemetry: on load mid-flight, or the link down.
    expect(tourCapturesPad(null, { live: false, armed: null })).toBe(false);
    expect(tourCapturesPad(null, { live: false, armed: false })).toBe(false);
    expect(tourCapturesPad(null, { live: true, armed: null })).toBe(false);
    expect(tourCapturesPad(null, { live: true, armed: true })).toBe(false);
    // A pilot that has not reported landed, or is armed on the ground.
    expect(tourCapturesPad({ armed: false, landed: false }, { live: true, armed: false })).toBe(false);
    expect(tourCapturesPad({ armed: true, landed: true }, { live: true, armed: true })).toBe(false);
  });
});
