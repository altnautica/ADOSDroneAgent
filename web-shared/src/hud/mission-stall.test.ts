import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import layout from "../cockpit-spec/layout.json";
import { parsePilotState, type MissionStall, type PilotState } from "./cockpit-frames";
import { gapReasonText, gapSeqs, missionStallActions, missionStallOverlay, missionStallVoiceKey, stallShadowsBanner } from "./mission-stall";
import { routeRoundPreview } from "./cockpit-mission";
import { specText } from "./spec-text";
import { pillAlerts } from "./status-pill";

// The golden stall frame the agent's own tests use, read in place.
const FIXTURES = path.resolve(__dirname, "../../../crates/ados-control/tests/fixtures");
const raw = JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame-stall.json"), "utf8")) as Record<string, unknown>;
const golden = parsePilotState(raw)!;
const plain = parsePilotState(JSON.parse(readFileSync(path.join(FIXTURES, "pilot-frame.json"), "utf8")))!;

const stall = (over: Partial<MissionStall> = {}): MissionStall => ({
  kind: "mission",
  seq: 3,
  wpLabel: "3",
  sinceMs: 0,
  skipInS: 32.5,
  returnInS: null,
  windowS: 15,
  ...over,
});

describe("the stall frame through the parser", () => {
  it("reads mission_stall camel case", () => {
    expect(golden.missionStall).toEqual({ kind: "mission", seq: 3, wpLabel: "3", sinceMs: 1793960000000, skipInS: 32.5, returnInS: null, windowS: 15 });
  });

  it("reads the mission target and gaps", () => {
    expect(golden.mission?.target).toEqual({ seq: 3, cmd: 16, ned: [42.5, -18, -15], radiusM: null });
    expect(golden.mission?.gaps).toEqual([{ seq: 2, reason: "stall_timeout", atMs: 1793959950000 }]);
  });

  it("reads no stall, no target and no gaps from the frame without one", () => {
    expect(plain.missionStall).toBeNull();
    expect(plain.mission?.target ?? null).toBeNull();
    expect(plain.mission?.gaps ?? []).toEqual([]);
  });

  it("drops a stall of an unknown kind and a gap without a seq", () => {
    expect(parsePilotState({ ...raw, mission_stall: { kind: "orbit", since_ms: 1, window_s: 15 } })!.missionStall).toBeNull();
    const m = parsePilotState({ ...raw, mission: { ...(raw.mission as object), gaps: [{ reason: "stall_timeout" }, { seq: 4 }] } })!.mission;
    expect(m?.gaps).toEqual([{ seq: 4, reason: null, atMs: null }]);
  });

  it("reads a return stall with no countdown", () => {
    const s = parsePilotState({ ...raw, mission_stall: { kind: "return", seq: null, wp_label: null, since_ms: 5, skip_in_s: null, window_s: 20 } })!.missionStall;
    expect(s).toEqual({ kind: "return", seq: null, wpLabel: null, sinceMs: 5, skipInS: null, returnInS: null, windowS: 20 });
  });
});

describe("the stall overlay", () => {
  it("renders the golden mission stall with its three actions", () => {
    const o = missionStallOverlay(golden.missionStall)!;
    expect(o.kind).toBe("mission");
    expect(o.severity).toBe("critical");
    expect(o.titleKey).toBe("flow.mission_stall.title");
    expect(o.titleArgs).toEqual({ wp: "3", window_s: 15, in_s: 33 });
    expect(o.title).toBe("No progress to waypoint 3 for 15 s · Skipping in 33 s");
    expect(o.actions.map((a) => a.action)).toEqual(["mission_skip", "rth", "land"]);
    expect(o.actions.map((a) => a.label)).toEqual(["Skip waypoint", "Return", "Land here"]);
  });

  it("holds for the operator with no countdown", () => {
    const o = missionStallOverlay(stall({ skipInS: null, windowS: 14.6 }))!;
    expect(o.titleKey).toBe("flow.mission_stall.title_hold");
    expect(o.title).toBe("No progress to waypoint 3 for 15 s · Holding for you");
  });

  it("reads a confined operator wait counting down to the return", () => {
    const s = parsePilotState({ ...raw, mission_stall: { kind: "mission", seq: 3, wp_label: "3", since_ms: 5, skip_in_s: null, return_in_s: 44.2, window_s: 15 } })!.missionStall;
    expect(s).toEqual({ kind: "mission", seq: 3, wpLabel: "3", sinceMs: 5, skipInS: null, returnInS: 44.2, windowS: 15 });
  });

  it("counts a confined operator wait down to the return home", () => {
    const o = missionStallOverlay(stall({ skipInS: null, returnInS: 44.2 }))!;
    expect(o.severity).toBe("critical");
    expect(o.titleKey).toBe("flow.mission_stall.title_return");
    expect(o.titleArgs).toEqual({ wp: "3", window_s: 15, in_s: 45 });
    expect(o.title).toBe("No progress to waypoint 3 for 15 s · No operator · Returning home in 45 s");
    expect(o.actions.map((a) => a.label)).toEqual(["Skip waypoint", "Return", "Land here"]);
  });

  it("words the return banners", () => {
    expect(specText("pilot.banner.mission_stalled_return", { wp: "3", in_s: 45 })).toBe("No progress to waypoint 3 · No operator · Returning home in 45 s");
    expect(specText("pilot.banner.mission_stalled_returning", { wp: "3" })).toBe("No operator answered · Returning home along the flown path (SmartRTL)");
    expect(specText("pilot.banner.mission_stalled_landing", { wp: "3" })).toBe("No operator answered · Position unverified · Landing here");
  });

  it("renders a blocked return with Land here only", () => {
    const o = missionStallOverlay(stall({ kind: "return", seq: null, wpLabel: null, skipInS: 44.1 }))!;
    expect(o.titleKey).toBe("flow.return_blocked.title");
    expect(o.title).toBe(specText("flow.return_blocked.title", { in_s: 45 }));
    expect(o.title).toBe("Return blocked · Landing in 45 s");
    expect(o.actions.map((a) => a.action)).toEqual(["land"]);
  });

  it("never counts below zero", () => {
    expect(missionStallOverlay(stall({ skipInS: -0.4 }))!.titleArgs.in_s).toBe(0);
  });

  it("is absent without a stall", () => {
    expect(missionStallOverlay(null)).toBeNull();
    expect(missionStallOverlay(plain.missionStall)).toBeNull();
  });

  it("matches the spec's severity for both kinds", () => {
    expect(layout.banners.severity.mission_stalled).toBe("critical");
    expect(layout.banners.severity.return_blocked).toBe("critical");
  });

  it("offers the actions per kind", () => {
    expect(missionStallActions("mission")).toEqual(["mission_skip", "rth", "land"]);
    expect(missionStallActions("return")).toEqual(["land"]);
  });
});

describe("the stall voice line", () => {
  const none: Pick<PilotState, "missionStall" | "banner"> = { missionStall: null, banner: null };

  it("speaks a stall once, when it appears", () => {
    expect(missionStallVoiceKey(none, golden)).toBe("voice.mission_stalled");
    expect(missionStallVoiceKey(null, golden)).toBe("voice.mission_stalled");
    expect(missionStallVoiceKey(golden, { ...golden, missionStall: stall({ skipInS: 20 }) })).toBeNull();
  });

  it("speaks a blocked return", () => {
    const ret = { missionStall: stall({ kind: "return" }), banner: null };
    expect(missionStallVoiceKey(none, ret)).toBe("voice.return_blocked");
    expect(missionStallVoiceKey(golden, ret)).toBe("voice.return_blocked");
    expect(specText("voice.return_blocked")).toBe("Return blocked. Holding.");
  });

  it("speaks a skipped waypoint on its banner", () => {
    const skipped = { missionStall: null, banner: { textKey: "pilot.banner.mission_wp_skipped", args: { wp: "3" }, untilMs: null } };
    expect(missionStallVoiceKey(golden, skipped)).toBe("voice.waypoint_skipped");
    expect(missionStallVoiceKey(skipped, skipped)).toBeNull();
    expect(missionStallVoiceKey(skipped, { ...skipped, banner: { ...skipped.banner, args: { wp: "4" } } })).toBe("voice.waypoint_skipped");
  });
});

describe("skipped waypoints", () => {
  it("lists the golden gap", () => {
    expect(gapSeqs(golden.mission)).toEqual([2]);
  });

  it("sorts and dedupes, and reads none without a mission", () => {
    expect(gapSeqs({ gaps: [{ seq: 5, reason: null, atMs: null }, { seq: 2, reason: null, atMs: null }, { seq: 5, reason: null, atMs: null }] })).toEqual([2, 5]);
    expect(gapSeqs(null)).toEqual([]);
  });

  it("words every gap reason the pilot sends", () => {
    expect(gapReasonText("unreachable_inside_obstacle")).toBe("Inside an obstacle");
    expect(gapReasonText("unreachable_no_path")).toBe("No way there within 200 m");
    expect(gapReasonText("stall_timeout")).toBe("No progress before the countdown");
    expect(gapReasonText("stall_operator")).toBe("Skipped by the operator");
    expect(gapReasonText("something_new")).toBe("something new");
    expect(gapReasonText(null)).toBeNull();
  });
});

describe("routing round", () => {
  const detour = { seq: 3, targetNed: [18.25, 0, -5] as [number, number, number], sinceMs: 1 };
  const routed = { remainingM: 38, straightM: 24, sinceMs: 1 };
  const none = { missionStall: null, banner: null, missionDetour: null, returnRoute: null };

  it("parses the detour and the routed return", () => {
    const p = parsePilotState({ ...raw, mission_detour: { seq: 3, target_ned: [18.25, 0, -5], since_ms: 1 }, return_route: { remaining_m: 38, straight_m: 24, since_ms: 1 } })!;
    expect(p.missionDetour).toEqual(detour);
    expect(p.returnRoute).toEqual(routed);
    expect(golden.missionDetour).toBeNull();
    expect(golden.returnRoute).toBeNull();
  });

  it("speaks a detour as it begins or moves on, and a routed return once", () => {
    expect(missionStallVoiceKey(none, { ...none, missionDetour: detour })).toBe("voice.mission_detour");
    expect(missionStallVoiceKey({ ...none, missionDetour: detour }, { ...none, missionDetour: detour })).toBeNull();
    expect(missionStallVoiceKey({ ...none, missionDetour: detour }, { ...none, missionDetour: { ...detour, seq: 4 } })).toBe("voice.mission_detour");
    expect(missionStallVoiceKey(none, { ...none, returnRoute: routed })).toBe("voice.return_routed");
    expect(missionStallVoiceKey({ ...none, returnRoute: routed }, { ...none, returnRoute: routed })).toBeNull();
  });

  it("speaks an unreachable waypoint on its banner", () => {
    for (const textKey of ["pilot.banner.mission_wp_inside_obstacle", "pilot.banner.mission_wp_no_path"]) {
      const b = { ...none, banner: { textKey, args: { wp: "4" }, untilMs: null } };
      expect(missionStallVoiceKey(none, b)).toBe("voice.waypoint_unreachable");
      expect(missionStallVoiceKey(b, b)).toBeNull();
    }
  });

  it("previews the planned way round, or the detour's free point alone", () => {
    const plan: [number, number][] = [[0, 0], [4, 3], [18.25, 0]];
    expect(routeRoundPreview({ missionDetour: detour, returnRoute: null, planNed: plan })).toEqual({ kind: "detour", pathNe: plan, targetNe: [18.25, 0] });
    expect(routeRoundPreview({ missionDetour: detour, returnRoute: null, planNed: [] })).toEqual({ kind: "detour", pathNe: [], targetNe: [18.25, 0] });
    expect(routeRoundPreview({ missionDetour: null, returnRoute: routed, planNed: plan })).toEqual({ kind: "return", pathNe: plan, targetNe: null });
    expect(routeRoundPreview({ missionDetour: null, returnRoute: routed, planNed: [] })).toBeNull();
    expect(routeRoundPreview({ missionDetour: null, returnRoute: null, planNed: plan })).toBeNull();
  });
});

describe("the stall in the status pill", () => {
  const base = { alerts: [], readiness: [], prearm: [] as string[], storagePct: 40 };

  it("raises one critical pilot item reading the overlay title", () => {
    const items = pillAlerts({ ...base, pilotAgeMs: 100, missionStall: golden.missionStall });
    expect(items).toEqual([{ id: "mission_stall", source: "pilot", severity: "critical", text: "No progress to waypoint 3 for 15 s · Skipping in 33 s" }]);
  });

  it("drops it with a stale pilot state or no stall", () => {
    expect(pillAlerts({ ...base, pilotAgeMs: null, missionStall: golden.missionStall }).map((a) => a.id)).toEqual(["pilot_not_answering"]);
    expect(pillAlerts({ ...base, pilotAgeMs: 100, missionStall: null })).toEqual([]);
  });
});

describe("the stall overlay shadows its banners", () => {
  const shadowed = ["pilot.banner.mission_stalled", "pilot.banner.mission_stalled_hold", "pilot.banner.mission_stalled_return", "pilot.banner.return_blocked"];

  it("hides the stall banners while the overlay shows", () => {
    for (const key of shadowed) expect(stallShadowsBanner(key, golden.missionStall)).toBe(true);
    expect(stallShadowsBanner(golden.banner?.textKey, golden.missionStall)).toBe(true);
    expect(stallShadowsBanner("pilot.banner.return_blocked", stall({ kind: "return" }))).toBe(true);
  });

  it("keeps every banner without a stall, and other banners with one", () => {
    for (const key of shadowed) expect(stallShadowsBanner(key, null)).toBe(false);
    expect(stallShadowsBanner("pilot.banner.route_infeasible", golden.missionStall)).toBe(false);
    expect(stallShadowsBanner("pilot.banner.mission_wp_skipped", golden.missionStall)).toBe(false);
    expect(stallShadowsBanner("pilot.banner.mission_stalled_returning", golden.missionStall)).toBe(false);
    expect(stallShadowsBanner("pilot.banner.mission_stalled_landing", golden.missionStall)).toBe(false);
    expect(stallShadowsBanner(null, golden.missionStall)).toBe(false);
  });
});
