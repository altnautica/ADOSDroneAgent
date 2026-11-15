import { readFileSync } from "node:fs";
import path from "node:path";

import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { CountdownCard } from "@/components/panels/countdown-card";
import { bannerText, countdownCard } from "@/lib/countdown-card";
import { homePill, type HomeStatusInputs } from "@/lib/home-status";
import layout from "@/shared/cockpit-spec/layout.json";
import { parsePilotState } from "@/shared/hud/cockpit-frames";

// The golden stall frame the agent's own tests use, read in place.
const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const raw = JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame-stall.json"), "utf8")) as Record<string, unknown>;
const golden = parsePilotState(raw)!;
/** Before the golden banner's slot ends. */
const NOW = 1793960001000;

const render = (pilot = golden, ageMs = 0, now = NOW): string => {
  const card = countdownCard(pilot, ageMs, now);
  return card ? renderToStaticMarkup(createElement(CountdownCard, { card, send: () => Promise.resolve({ ok: true }) })) : "";
};

describe("the countdown card on the golden stall frame", () => {
  it("shows the stall, critical, with Skip waypoint, Return and Land here", () => {
    const card = countdownCard(golden, 0, NOW)!;
    expect(card.stall?.title).toBe("No progress to waypoint 3 for 15 s · Skipping in 33 s");
    expect(card.stall?.severity).toBe("critical");
    expect(card.stall?.actions.map((a) => a.action)).toEqual(["mission_skip", "rth", "land"]);
    const html = render();
    expect(html).toContain('data-testid="countdown-stall"');
    expect(html).toContain("No progress to waypoint 3 for 15 s · Skipping in 33 s");
    for (const [action, label] of [
      ["mission_skip", "Skip waypoint"],
      ["rth", "Return"],
      ["land", "Land here"],
    ]) {
      expect(html).toMatch(new RegExp(`data-action="${action}"[^>]*>${label}</button>`));
    }
  });

  it("hides the stall's own banner while the stall shows", () => {
    expect(golden.banner?.textKey).toBe("pilot.banner.mission_stalled");
    expect(countdownCard(golden, 0, NOW)?.banner).toBeNull();
    expect(render()).not.toContain('data-testid="countdown-banner"');
    const blocked = parsePilotState({ ...raw, banner: { text_key: "pilot.banner.return_blocked", args: { in_s: 12 }, until_ms: null }, mission_stall: { kind: "return", seq: null, wp_label: null, since_ms: 1, skip_in_s: 12, window_s: 20 } })!;
    expect(countdownCard(blocked, 0, NOW)?.banner).toBeNull();
  });

  it("shows the pilot's banner in the spec's words while its slot lasts", () => {
    const unstalled = { ...golden, missionStall: null };
    expect(countdownCard(unstalled, 0, NOW)?.banner).toBe("No progress to waypoint 3 · Holding · Skipping in 33 s");
    expect(render(unstalled)).toContain('data-testid="countdown-banner"');
    expect(countdownCard(unstalled, 0, 1793960003001)).toBeNull();
  });

  it("counts the stall down by the frame's age between polls", () => {
    expect(countdownCard(golden, 2500, NOW)?.stall?.title).toBe("No progress to waypoint 3 for 15 s · Skipping in 30 s");
  });

  it("offers Land here only for a blocked return", () => {
    const ret = parsePilotState({ ...raw, banner: null, mission_stall: { kind: "return", seq: null, wp_label: null, since_ms: 1, skip_in_s: 12, window_s: 20 } })!;
    const card = countdownCard(ret, 0, NOW)!;
    expect(card.stall?.title).toBe("Return blocked · Landing in 12 s");
    expect(card.stall?.actions.map((a) => a.action)).toEqual(["land"]);
    expect(render(ret)).not.toContain('data-action="rth"');
  });

  it("is absent with nothing to count or say", () => {
    const quiet = parsePilotState({ ...raw, banner: null, mission_stall: null, rth_countdown_s: null })!;
    expect(countdownCard(quiet, 0, NOW)).toBeNull();
    expect(countdownCard(null, 0, NOW)).toBeNull();
  });
});

describe("the low-battery return countdown", () => {
  const low = parsePilotState({
    ...raw,
    mission_stall: null,
    banner: { text_key: "pilot.banner.low_battery_rth", args: { pct: 22, need_pct: 25, in_s: 10 }, until_ms: null },
    rth_countdown_s: 9.2,
  })!;

  it("shows the countdown, its warning and Cancel", () => {
    const card = countdownCard(low, 0, NOW)!;
    expect(card.lowBattery?.title).toBe("Battery reserve reached · Returning along the flown path in 10 s");
    expect(card.lowBattery?.cancel).toEqual({ action: "rth_cancel", label: "Cancel" });
    expect(card.banner).toBe("Battery reserve 22% (need 25%) · Returning in 10 s");
    const html = render(low);
    expect(html).toContain('data-testid="countdown-low-battery"');
    expect(html).toMatch(/data-action="rth_cancel"[^>]*>Cancel<\/button>/);
    expect(layout.banners.severity.low_battery_rth).toBe("critical");
  });

  it("never counts below zero", () => {
    expect(countdownCard(low, 20_000, NOW)?.lowBattery?.title).toBe("Battery reserve reached · Returning along the flown path in 0 s");
  });
});

describe("banner causes", () => {
  it("read in words, or a dash for an unknown code", () => {
    const b = (cause: string) => ({ textKey: "pilot.banner.fc_mode_change", args: { mode: "RTL", cause }, untilMs: null });
    expect(bannerText(b("fc"))).toBe("FC switched to RTL: flight controller");
    expect(bannerText(b("new_code"))).toBe("FC switched to RTL: —");
  });
});

describe("the stall in the Home status pill", () => {
  const inputs: HomeStatusInputs = {
    pilot: golden,
    telemetry: null,
    battery: null,
    status: null,
    live: true,
    msSinceLive: 0,
    armedAtLastLive: true,
    pilotAgeMs: 100,
    agentReachable: true,
  };

  it("raises one critical item reading the stall", () => {
    const pill = homePill(inputs);
    const item = pill.alerts.find((a) => a.id === "mission_stall");
    expect(item).toEqual({ id: "mission_stall", source: "pilot", severity: "critical", text: "No progress to waypoint 3 for 15 s · Skipping in 33 s" });
    expect(pill.severity).toBe("critical");
  });

  it("drops it once the pilot frame is stale", () => {
    expect(homePill({ ...inputs, pilotAgeMs: 5000 }).alerts.some((a) => a.id === "mission_stall")).toBe(false);
  });
});
