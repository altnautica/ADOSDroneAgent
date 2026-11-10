import { describe, expect, it } from "vitest";

import tipsJson from "../cockpit-spec/tips.json";
import tipsCases from "./fixtures/tips-cases.json";
import {
  dismissTip,
  hiddenThisOpenAfter,
  nextTip,
  onFlightScreenOpen,
  placeTip,
  resetTips,
  TIPS,
  type TipDecision,
  type TipRect,
  type TipsContext,
  type TipsState,
} from "./tips";

const R = tipsJson.rules;
const ANCHORS = Object.fromEntries(tipsJson.tips.map((t) => [t.anchor, true]));

function ctx(over: Partial<TipsContext> = {}): TipsContext {
  return {
    armed: false,
    airborne: false,
    flightScreen: true,
    sheetOrMenuOpen: false,
    tourVisible: false,
    skillsAvailable: false,
    gotoOffered: false,
    gimbalAvailable: false,
    anchorsVisible: ANCHORS,
    screenOpenedAtMs: 0,
    lastDismissedAtMs: null,
    shownTipId: null,
    shownAtMs: null,
    ...over,
  };
}

const fresh: TipsState = { seen: [], openCount: 1 };

describe("tips queue", () => {
  it("runs in the spec's order", () => {
    expect(TIPS.map((t) => t.id)).toEqual([
      "status_pill",
      "flight_state_chip",
      "layer_level",
      "stop",
      "skills_chip",
      "fly_here",
      "gimbal_pad",
      "card_switch",
    ]);
  });

  it("waits the first delay after the screen opens, then shows the first tip", () => {
    expect(nextTip(fresh, ctx(), 0)).toEqual({ kind: "wait", tipId: "status_pill", atMs: R.first_delay_ms });
    expect(nextTip(fresh, ctx(), R.first_delay_ms)).toEqual({ kind: "show", tipId: "status_pill", textKey: "tips.status_pill", anchor: "status_pill" });
  });

  it("waits the gap after a dismissal before the next", () => {
    const s = dismissTip(fresh, "status_pill");
    const c = ctx({ lastDismissedAtMs: 10_000 });
    expect(nextTip(s, c, 10_000)).toEqual({ kind: "wait", tipId: "flight_state_chip", atMs: 10_000 + R.gap_between_ms });
    expect(nextTip(s, c, 10_000 + R.gap_between_ms)).toMatchObject({ kind: "show", tipId: "flight_state_chip" });
  });

  it("shows nothing armed, airborne, under a sheet or the tour, or off the flight screen", () => {
    for (const over of [{ armed: true }, { airborne: true }, { sheetOrMenuOpen: true }, { tourVisible: true }, { flightScreen: false }]) {
      expect(nextTip(fresh, ctx(over), 60_000)).toEqual({ kind: "none" });
    }
  });

  it("skips a tip whose anchor is not on screen without marking it seen", () => {
    const d = nextTip(fresh, ctx({ anchorsVisible: { ...ANCHORS, status_pill: false } }), 60_000);
    expect(d).toMatchObject({ kind: "show", tipId: "flight_state_chip" });
  });

  it("auto-dismisses a tip without marking it seen", () => {
    const c = ctx({ shownTipId: "status_pill", shownAtMs: 1000 });
    expect(nextTip(fresh, c, 1000 + R.auto_dismiss_ms - 1)).toMatchObject({ kind: "show", tipId: "status_pill" });
    expect(nextTip(fresh, c, 1000 + R.auto_dismiss_ms)).toEqual({ kind: "hide", tipId: "status_pill", markSeen: false, reason: "auto_dismiss" });
    // Not offered again on this open.
    expect(nextTip(fresh, ctx({ hiddenThisOpen: ["status_pill"] }), 60_000)).toMatchObject({ tipId: "flight_state_chip" });
  });

  it("hides the visible tip and marks it seen on arming", () => {
    expect(nextTip(fresh, ctx({ armed: true, shownTipId: "stop", shownAtMs: 0 }), 100)).toEqual({ kind: "hide", tipId: "stop", markSeen: true, reason: "arm" });
  });

  it("hides the visible tip unseen when a sheet opens", () => {
    expect(nextTip(fresh, ctx({ sheetOrMenuOpen: true, shownTipId: "stop", shownAtMs: 0 }), 100)).toEqual({
      kind: "hide",
      tipId: "stop",
      markSeen: false,
      reason: "not_allowed",
    });
  });

  it("lists only an auto-dismissed tip as hidden for this open; a banner hide requeues it", () => {
    const autoHide = nextTip(fresh, ctx({ shownTipId: "status_pill", shownAtMs: 1000 }), 1000 + R.auto_dismiss_ms);
    expect(hiddenThisOpenAfter(autoHide)).toBe(true);
    const bannerHide = nextTip(fresh, ctx({ shownTipId: "status_pill", shownAtMs: 1000, alertBannerVisible: true }), 2000);
    expect(bannerHide).toMatchObject({ kind: "hide", reason: "not_allowed" });
    expect(hiddenThisOpenAfter(bannerHide)).toBe(false);
    expect(hiddenThisOpenAfter(nextTip(fresh, ctx({ armed: true, shownTipId: "status_pill", shownAtMs: 1000 }), 2000))).toBe(false);
    // Back at the head of the queue, after the gap.
    expect(nextTip(fresh, ctx({ lastDismissedAtMs: 2000 }), 2000 + R.gap_between_ms)).toMatchObject({ kind: "show", tipId: "status_pill" });
  });

  it("fires the conditional triggers", () => {
    const firstDone: TipsState = { seen: ["status_pill", "flight_state_chip", "layer_level", "stop"], openCount: 1 };
    expect(nextTip(firstDone, ctx(), 60_000)).toEqual({ kind: "none" });
    expect(nextTip(firstDone, ctx({ skillsAvailable: true }), 60_000)).toMatchObject({ kind: "show", tipId: "skills_chip", anchor: "skill_picker_chip" });
    expect(nextTip(firstDone, ctx({ gotoOffered: true }), 60_000)).toMatchObject({ kind: "show", tipId: "fly_here", anchor: "bottom_left_card" });
    expect(nextTip(firstDone, ctx({ gimbalAvailable: true }), 60_000)).toMatchObject({ kind: "show", tipId: "gimbal_pad" });
    expect(nextTip(onFlightScreenOpen(firstDone), ctx(), 60_000)).toMatchObject({ kind: "show", tipId: "card_switch" });
  });

  it("keeps state pure and resets to nothing seen", () => {
    expect(onFlightScreenOpen({ seen: [], openCount: 0 })).toEqual({ seen: [], openCount: 1 });
    expect(dismissTip(dismissTip(fresh, "stop"), "stop")).toEqual({ seen: ["stop"], openCount: 1 });
    expect(resetTips()).toEqual({ seen: [], openCount: 0 });
    expect(nextTip(resetTips(), ctx(), 60_000)).toMatchObject({ kind: "show", tipId: "status_pill" });
  });
});

describe("tip placement", () => {
  const viewport = { x: 0, y: 0, w: 1000, h: 600 };
  const tip = { w: 200, h: 60 };

  it("goes on the side with the most room", () => {
    // Anchor at the top: below has the room.
    expect(placeTip({ x: 400, y: 10, w: 100, h: 40 }, tip, viewport, [])).toEqual({ x: 350, y: 58, side: "bottom" });
    // Anchor at the right edge, mid height: left has the room.
    expect(placeTip({ x: 940, y: 280, w: 50, h: 40 }, tip, viewport, [])?.side).toBe("left");
  });

  it("clamps inside the viewport and slides clear of the avoided rects", () => {
    const p = placeTip({ x: 0, y: 10, w: 60, h: 40 }, tip, viewport, [{ x: 0, y: 50, w: 150, h: 200 }]);
    expect(p).not.toBeNull();
    const rect = { ...p!, w: tip.w, h: tip.h };
    expect(rect.x).toBeGreaterThanOrEqual(0);
    expect(rect.x + rect.w <= 0 || rect.x >= 150 || rect.y >= 250 || rect.y + rect.h <= 50).toBe(true);
  });

  it("is null when no side fits", () => {
    expect(placeTip({ x: 0, y: 0, w: 1000, h: 600 }, tip, viewport, [])).toBeNull();
  });
});

describe("tips golden cases (fixtures/tips-cases.json)", () => {
  // A case with `input.place` checks placement; every other case the queue.
  for (const c of tipsCases as { name: string; input: Record<string, unknown>; expected: unknown }[]) {
    it(c.name, () => {
      const i = c.input as {
        place?: { anchor: TipRect; tip: { w: number; h: number }; viewport: TipRect; avoid: TipRect[] };
        state: TipsState;
        ctx: TipsContext;
        now_ms: number;
      };
      if (i.place) {
        expect(placeTip(i.place.anchor, i.place.tip, i.place.viewport, i.place.avoid)).toEqual(c.expected);
        return;
      }
      const d: TipDecision = nextTip(i.state, i.ctx, i.now_ms);
      expect(d).toEqual(c.expected);
    });
  }
});
