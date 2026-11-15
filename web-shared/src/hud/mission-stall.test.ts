import { readFileSync } from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

import layout from "../cockpit-spec/layout.json";
import { parsePilotState, type MissionStall, type PilotState } from "./cockpit-frames";
import { gapSeqs, missionStallActions, missionStallOverlay, missionStallVoiceKey } from "./mission-stall";
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
  windowS: 15,
  ...over,
});

describe("the stall frame through the parser", () => {
  it("reads mission_stall camel case", () => {
    expect(golden.missionStall).toEqual({ kind: "mission", seq: 3, wpLabel: "3", sinceMs: 1793960000000, skipInS: 32.5, windowS: 15 });
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
    expect(s).toEqual({ kind: "return", seq: null, wpLabel: null, sinceMs: 5, skipInS: null, windowS: 20 });
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
