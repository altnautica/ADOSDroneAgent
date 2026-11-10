import { describe, expect, it } from "vitest";

import flightState from "../cockpit-spec/flight-state.json";
import layout from "../cockpit-spec/layout.json";
import strings from "../cockpit-spec/setup-strings.json";
import tips from "../cockpit-spec/tips.json";
import views from "../cockpit-spec/views.json";
import holdReasons from "./fixtures/hold-reasons.json";

const S: Record<string, string> = strings.strings;
const HOLD: Record<string, string> = flightState.hold_reason_labels;

describe("hold reasons", () => {
  it("labels every reason the pilot emits, by its wire string, the same in both files", () => {
    for (const { reason } of holdReasons) {
      expect(HOLD[reason], `flight-state.json hold_reason_labels.${reason}`).toBeTruthy();
      expect(S[`pilot.hold_reason.${reason}`], `setup-strings.json pilot.hold_reason.${reason}`).toBe(HOLD[reason]);
    }
  });

  it("keys labels by the hyphenated wire form only", () => {
    for (const key of Object.keys(HOLD)) expect(key).not.toMatch(/_/);
  });
});

describe("tips", () => {
  it("has a text for every tip and for the shared buttons", () => {
    for (const t of tips.tips) expect(S[`tips.${t.id}`], `tips.${t.id}`).toBeTruthy();
    expect(S[tips.rules.reset_setting_key]).toBe("Reset tips");
    expect(S["tips.dismiss"]).toBe("Got it");
  });

  it("orders tips uniquely and anchors them to layout zones or items", () => {
    const orders = tips.tips.map((t) => t.order);
    expect(new Set(orders).size).toBe(orders.length);
    const known = new Set<string>(["layer_button", "stop", "status_pill", "flight_state_chip", "gimbal_pad", "skill_picker_chip", "bottom_left_card"]);
    for (const z of layout.zones) {
      known.add(z.id);
      for (const item of ("items" in z ? z.items : []) as string[]) known.add(item);
    }
    for (const t of tips.tips) expect(known.has(t.anchor), t.anchor).toBe(true);
  });

  it("never covers the safety zones", () => {
    expect(tips.rules.never_cover_zones).toEqual(["stop", "left_column", "flight_state_chip"]);
  });
});

describe("controller tour buttons", () => {
  it("are spec strings", () => {
    for (const k of ["next", "done", "skip", "replay"]) expect(S[`controls.tour.${k}`], k).toBeTruthy();
  });
});

describe("layout values", () => {
  it("carries the shared thresholds and holds", () => {
    expect(layout.status_pill.storage_warn_pct).toBe(90);
    expect(layout.status_pill.storage_crit_pct).toBe(97);
    expect(layout.battery.low_fallback_pct).toBe(20);
    expect(layout.holds_ms.stop).toBe(3000);
    expect(layout.holds_ms.take_control).toBe(1500);
    expect(layout.launch.alt_min_m).toBe(1);
    expect(layout.launch.alt_max_m).toBe(120);
    expect(layout.map_long_press_ms).toBeGreaterThan(0);
  });

  it("names every menu entry and overflow item once", () => {
    const top = layout.zones.find((z) => z.id === "top_bar") as { overflow: { to_menu_order: string[]; never_move: string[] } };
    expect(top.overflow.to_menu_order).toEqual(["controller", "nav_source", "link_text", "link_icon", "layer_button"]);
    for (const id of top.overflow.never_move) expect(top.overflow.to_menu_order).not.toContain(id);
    expect(new Set(layout.menu.order).size).toBe(layout.menu.order.length);
  });
});

describe("views", () => {
  it("puts the map view's video in the bottom-left card and makes the split draggable", () => {
    const map = views.views.find((v) => v.id === "map") as { video_slot?: string };
    expect(map.video_slot).toBe("bottom_left_card");
    const split = views.views.find((v) => v.id === "split") as { divider?: { default: number; min: number; max: number } };
    expect(split.divider).toMatchObject({ default: 0.5, min: 0.3, max: 0.7 });
  });
});
