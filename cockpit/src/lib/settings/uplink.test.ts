import { describe, expect, it } from "vitest";

import { UPLINK_UI_IDLE, legStates, moveEntry, shareOutcome, uplinkCommit, uplinkReducer } from "./uplink";

describe("legStates", () => {
  it("reports unknown legs as not reported and marks the agent's active uplink", () => {
    const legs = legStates({ active_uplink: "eth0" });
    expect(legs.map((l) => [l.leg, l.known, l.active])).toEqual([
      ["ethernet", true, true],
      ["wifi", false, false],
      ["cellular", false, false],
      ["usb", false, false],
      ["ap", false, false],
    ]);
    expect(legs[0].state).toEqual({ key: "uplink.state.carryingTraffic" });
  });

  it("details a connected wifi leg and renders a raw modem state", () => {
    const legs = legStates({
      wifi_client: { connected: true, ssid: "field", signal: 70, ip: "10.0.0.5" },
      modem_4g: { enabled: true, state: "registered", percent: 42.4 },
      ap: { enabled: true, standing_down: true, ssid: "node-ap" },
    });
    expect(legs[1]).toMatchObject({ known: true, detail: "field · 70% · 10.0.0.5" });
    expect(legs[2]).toMatchObject({ state: { raw: "registered" }, detail: "42%" });
    expect(legs[4].state).toEqual({ key: "uplink.state.standingDown", params: { reason: null } });
  });
});

describe("moveEntry", () => {
  it("moves within range and refuses out of range", () => {
    expect(moveEntry(["a", "b", "c"], 2, -1)).toEqual(["a", "c", "b"]);
    expect(moveEntry(["a", "b"], 0, -1)).toBeNull();
    expect(moveEntry(["a", "b"], 1, 1)).toBeNull();
  });
});

describe("uplink gate", () => {
  const base = ["eth0", "wlan0_client", "wwan0"];

  it("a reorder is a draft until the slide commits it", () => {
    const moved = uplinkReducer(UPLINK_UI_IDLE, { type: "move", base, index: 1, delta: -1 });
    expect(moved.draft).toEqual(["wlan0_client", "eth0", "wwan0"]);
    expect(uplinkCommit(moved, "shareSlide")).toBeNull();
    expect(uplinkCommit(moved, "prioritySlide")).toEqual({
      kind: "priority",
      priority: ["wlan0_client", "eth0", "wwan0"],
    });
  });

  it("moving back to the agent's order clears the draft, so a slide sends nothing", () => {
    const there = uplinkReducer(UPLINK_UI_IDLE, { type: "move", base, index: 0, delta: 1 });
    const back = uplinkReducer(there, { type: "move", base, index: 1, delta: -1 });
    expect(back.draft).toBeNull();
    expect(uplinkCommit(back, "prioritySlide")).toBeNull();
    expect(uplinkCommit(uplinkReducer(there, { type: "revert" }), "prioritySlide")).toBeNull();
  });

  it("a share toggle is staged for its slide; cancel drops it", () => {
    const staged = uplinkReducer(UPLINK_UI_IDLE, { type: "stageShare", enabled: true });
    expect(uplinkCommit(staged, "prioritySlide")).toBeNull();
    expect(uplinkCommit(staged, "shareSlide")).toEqual({ kind: "share", enabled: true });
    expect(uplinkCommit(uplinkReducer(staged, { type: "cancelShare" }), "shareSlide")).toBeNull();
    expect(uplinkCommit(UPLINK_UI_IDLE, "shareSlide")).toBeNull();
  });
});

describe("shareOutcome", () => {
  it("flags a persisted but unapplied share", () => {
    expect(shareOutcome({ enabled: true, applied: false, apply_error: "iptables" })).toEqual({
      ok: false,
      key: "uplink.toast.notApplied",
      params: { error: "iptables" },
    });
    expect(shareOutcome({ enabled: false })).toEqual({ ok: true, key: "uplink.toast.shareDisabled" });
  });
});
