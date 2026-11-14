import { describe, expect, it } from "vitest";

import builtins from "../cockpit-spec/builtins.json";
import flightState from "../cockpit-spec/flight-state.json";
import layout from "../cockpit-spec/layout.json";
import strings from "../cockpit-spec/setup-strings.json";
import tips from "../cockpit-spec/tips.json";
import tokens from "../cockpit-spec/tokens.json";
import views from "../cockpit-spec/views.json";
import holdReasons from "./fixtures/hold-reasons.json";
import { PILL_SOURCES, severityTextColor } from "./status-pill";

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
    expect(tips.rules.never_cover_zones).toEqual(["stop", "left_column", "flight_state_chip", "alert_stack", "top_bar"]);
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

describe("emergency stop", () => {
  it("is allowed in any arm state and always holds 3 s before it sends", () => {
    const kill = builtins.builtins.find((b) => b.id === "kill");
    expect(kill?.arm_requirement).toBe("any");
    expect(kill?.confirm).toEqual({ kind: "hold_confirm", ms: layout.holds_ms.stop });
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

/** WCAG relative luminance of #RRGGBB. */
function lum(hex: string): number {
  const c = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255);
  const [r, g, b] = c.map((v) => (v <= 0.03928 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4));
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

function contrast(a: string, b: string): number {
  const [hi, lo] = [lum(a), lum(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

/** #RRGGBBAA panel composited over an opaque #RRGGBB backdrop. */
function over(panel: string, backdrop: string): string {
  const a = parseInt(panel.slice(7, 9), 16) / 255;
  const ch = (i: number) => Math.round(parseInt(panel.slice(i, i + 2), 16) * a + parseInt(backdrop.slice(i, i + 2), 16) * (1 - a));
  return "#" + [1, 3, 5].map((i) => ch(i).toString(16).padStart(2, "0")).join("");
}

describe("flight-screen readability over bright video", () => {
  const panelOnWhite = over(tokens.chrome.panel_bg, "#FFFFFF");

  it("keeps panel text at 4.5:1 or more over a white picture", () => {
    expect(contrast(tokens.chrome.text, panelOnWhite)).toBeGreaterThanOrEqual(4.5);
    expect(contrast(tokens.chrome.text_critical, panelOnWhite)).toBeGreaterThanOrEqual(4.5);
  });

  it("draws severity words in a text colour that keeps 4.5:1, never the dot hue", () => {
    for (const s of ["ok", "warning", "critical"] as const) {
      expect(contrast(severityTextColor(s), panelOnWhite)).toBeGreaterThanOrEqual(4.5);
    }
    expect(severityTextColor("critical")).toBe(tokens.chrome.text_critical);
    expect(contrast(tokens.chrome.severity_critical, panelOnWhite)).toBeLessThan(4.5);
  });

  it("gives the STOP label 4.5:1 on its fill", () => {
    expect(contrast(tokens.chrome.stop_text, tokens.chrome.stop_bg)).toBeGreaterThanOrEqual(4.5);
  });

  it("lifts tv labels to 16 px", () => {
    expect(layout.classes.tv.label_min_px).toBe(16);
  });
});

describe("flight-screen sheets", () => {
  it("start below the top bar and never cover STOP", () => {
    expect(layout.sheets).toMatchObject({ top_inset: "top_bar", never_cover_zones: ["stop"], scrim_excludes: "top_bar" });
  });
});

describe("top bar cells", () => {
  const top = layout.zones.find((z) => z.id === "top_bar") as unknown as {
    chip: { max_frac: number };
    link: { text: string; never: string };
    battery: { time_to_reserve: { text: string; short: string } };
  };

  it("caps the chip width", () => {
    expect(top.chip.max_frac).toBe(0.4);
    expect(S["chip.full_text"]).toBe("Flight state: {text}");
  });

  it("names the link cell metric and the reserve time", () => {
    expect(S[top.link.text]).toBe("{s} s");
    expect(S[top.link.never]).toBeTruthy();
    expect(S[top.battery.time_to_reserve.text]).toBe("{t} to reserve");
    expect(S[top.battery.time_to_reserve.short]).toBe("{t} RES");
  });
});

describe("left column", () => {
  it("labels every built-in with a short label from the spec", () => {
    for (const b of builtins.builtins) {
      const short = (b as { short_label?: string }).short_label;
      expect(short, b.id).toBeTruthy();
      expect(S[`builtin.short.${b.id}`]).toBe(short);
    }
    const left = layout.zones.find((z) => z.id === "left_column") as unknown as { labels: { show_at: string[] } };
    expect(left.labels.show_at).toEqual(["medium", "expanded", "tv"]);
  });
});

describe("status pill sources", () => {
  it("covers a silent pilot, a lost controller and failing pre-flight checks", () => {
    expect(layout.status_pill.sources).toEqual(expect.arrayContaining(["pilot", "controller", "preflight"]));
    for (const k of ["alert.pilot_not_answering", "alert.controller_lost", "alert.agent_unreachable", "alert.preflight_failing"]) {
      expect(S[k], k).toBeTruthy();
    }
    expect(S[layout.status_pill.storage_text]).toBe("{pct} % used");
  });

  it("words every pill source and lists it in the layout", () => {
    for (const s of PILL_SOURCES) {
      expect(S[`cockpit.status.source.${s}`], s).toBeTruthy();
    }
    expect([...layout.status_pill.sources].sort()).toEqual([...PILL_SOURCES].sort());
    expect(S["cockpit.status.source.video"]).toBe("Video");
  });

  it("colours both countdowns critical", () => {
    expect(layout.banners.severity).toEqual({ low_battery_rth: "critical", link_loss: "critical" });
  });
});

describe("tips stay clear of alerts", () => {
  it("never cover the alert stack and wait while an alert banner shows", () => {
    expect(tips.rules.never_cover_zones).toContain("alert_stack");
    expect(tips.rules.show_only_when.alert_banner_visible).toBe(false);
    expect(layout.zones.some((z) => z.id === "alert_stack")).toBe(true);
  });

  it("use verbs that fit touch, mouse and pad", () => {
    for (const t of tips.tips) expect(S[`tips.${t.id}`], t.id).not.toMatch(/\bTap\b/);
  });
});

describe("words every surface shares", () => {
  it("has video state words, cause labels and the new chip and hold texts", () => {
    for (const k of [
      "video.state.connecting",
      "video.state.no_source",
      "video.state.failed",
      "video.state.reconnecting",
      "pilot.banner.cause.rc_or_failsafe",
      "pilot.banner.cause.fc",
      "chip.failsafe",
      "chip.mission_paused_line",
      "chip.taking_off",
      "hold.keep_holding",
      "setup.banner.title",
      "setup.banner.open",
      "setup.banner.later",
      "refusal.landed_unknown",
      "nav.reduced_rate",
      "shell.agent_offline",
      "controls.sync.refused",
    ]) {
      expect(S[k], k).toBeTruthy();
    }
  });

  it("has the pending, photo taken and command timeout words", () => {
    expect(S["refusal.pending"]).toBe("Waiting for the last answer");
    expect(S["camera.capture_sent"]).toBe("Photo taken");
    expect(S["mav_result.TIMEOUT"]).toBe(S["refusal.no_response"]);
    expect(S["flow.mode.stale"]).toBe("No answer from ADOS Pilot: current values unknown (—)");
    expect(S["refusal.not_authorized"]).toBe("Not authorized on this drone");
  });
});

describe("cockpit panel words", () => {
  it("covers the follow rail, PiP, vision panel, radar, re-acquire, map status and nav-source toast", () => {
    for (const k of [
      "follow.tracking_close",
      "follow.swap_cameras",
      "cockpit.pip.swap_leg",
      "vision.on_while_view",
      "vision.no_backend",
      "vision.fps",
      "vision.idle",
      "radar.title",
      "radar.no_heading",
      "radar.range_change",
      "target.reacquire_mode",
      "nav_source.no_ack",
      "map.status.memory",
      "map.status.upload",
      "map.status.live_stream",
      "map.status.placement.onboard",
      "map.status.upload.uploading",
      "map.status.stream.degraded",
    ]) {
      expect(S[k], k).toBeTruthy();
    }
  });
  it("has the GNSS-denied speed-cap words", () => {
    expect(S["pilot.speed_cap.gnss_denied"]).toBe("GNSS-denied: {v} m/s max");
  });
});
