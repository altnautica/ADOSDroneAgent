import { describe, expect, it } from "vitest";

import { ApiError } from "@/shared/api-fetch";
import type { WifiNetwork } from "@/shared/api/types";

import {
  JOIN_CLOSED,
  joinReducer,
  joinRequest,
  manualTarget,
  mergeNetworks,
  needsForce,
  type JoinAction,
  type JoinState,
  type JoinTarget,
} from "./wifi";

const net = (ssid: string, signal: number, extra: Partial<WifiNetwork> = {}): WifiNetwork => ({
  ssid,
  bssid: `${ssid}-${signal}`,
  signal,
  security: "WPA2",
  in_use: false,
  ...extra,
});

const secured: JoinTarget = { ssid: "field", security: "WPA2", saved: false };

function run(actions: JoinAction[], from: JoinState = JOIN_CLOSED): JoinState {
  return actions.reduce(joinReducer, from);
}

/** The native agent's AP-busy refusal. */
const apBusy = new ApiError("409 Conflict", 409, {
  detail: { error: { code: "E_WLAN0_BUSY_AP_ACTIVE", message: "AP is active; retry with force=true" } },
  needs_force: true,
});

describe("mergeNetworks", () => {
  it("keeps the strongest BSSID per SSID and inherits in_use", () => {
    const merged = mergeNetworks([net("a", 40, { in_use: true }), net("a", 80), net("b", 60), net("", 90)]);
    expect(merged.map((n) => [n.ssid, n.signal, n.in_use])).toEqual([
      ["", 90, false],
      ["a", 80, true],
      ["b", 60, false],
    ]);
  });
});

describe("manualTarget", () => {
  it("trims, rejects blank, and marks a saved profile", () => {
    expect(manualTarget("   ", new Set())).toBeNull();
    expect(manualTarget(" hidden ", new Set(["hidden"]))).toEqual({ ssid: "hidden", security: "WPA2", saved: true });
  });
});

describe("needsForce", () => {
  it("reads the top-level and the nested flag, and the hint", () => {
    expect(needsForce(apBusy)).toEqual({ message: "AP is active; retry with force=true" });
    expect(needsForce(new ApiError("409", 409, { detail: { needs_force: true } }))).toEqual({ message: null });
  });

  it("ignores other conflicts and statuses", () => {
    expect(needsForce(new ApiError("409", 409, { error: "E_ARMED" }))).toBeNull();
    expect(needsForce(new ApiError("500", 500, { needs_force: true }))).toBeNull();
    expect(needsForce(new Error("boom"))).toBeNull();
  });
});

describe("join flow", () => {
  it("sends nothing until the join slide", () => {
    const typed = run([{ type: "open", target: secured }, { type: "password", value: "hunter22" }]);
    expect(typed.phase).toBe("password");
    expect(joinRequest(typed)).toBeNull();

    const sliding = joinReducer(typed, { type: "slide" });
    expect(joinRequest(sliding)).toEqual({ ssid: "field", passphrase: "hunter22", force: false });
  });

  it("blocks the slide on a new secured network with no password", () => {
    const blocked = run([{ type: "open", target: secured }, { type: "slide" }]);
    expect(blocked).toMatchObject({ phase: "password", error: { key: "wifi.join.passwordRequired" } });
    expect(joinRequest(blocked)).toBeNull();
  });

  it("reuses saved credentials and sends no passphrase to an open network", () => {
    const saved = run([{ type: "open", target: { ...secured, saved: true } }, { type: "slide" }]);
    expect(joinRequest(saved)).toEqual({ ssid: "field", passphrase: null, force: false });

    const open = run([
      { type: "open", target: { ssid: "cafe", security: "--", saved: false } },
      { type: "password", value: "ignored" },
      { type: "slide" },
    ]);
    expect(joinRequest(open)).toEqual({ ssid: "cafe", passphrase: null, force: false });
  });

  it("re-prompts on 409 needs_force and sends force only after the second slide", () => {
    const refused = run([
      { type: "open", target: secured },
      { type: "password", value: "hunter22" },
      { type: "slide" },
      { type: "failed", err: apBusy },
    ]);
    expect(refused).toMatchObject({ phase: "force", message: "AP is active; retry with force=true" });
    expect(joinRequest(refused)).toBeNull();

    // The first slide's event does nothing in the force phase.
    expect(joinRequest(joinReducer(refused, { type: "slide" }))).toBeNull();

    const forced = joinReducer(refused, { type: "forceSlide" });
    expect(joinRequest(forced)).toEqual({ ssid: "field", passphrase: "hunter22", force: true });
  });

  it("does not loop on a needs_force answer to a forced join", () => {
    const forced: JoinState = { phase: "joining", target: secured, password: "hunter22", force: true };
    expect(joinReducer(forced, { type: "failed", err: apBusy })).toMatchObject({ phase: "password", error: null });
  });

  it("a force slide outside the force phase sends nothing", () => {
    const typed = run([{ type: "open", target: secured }, { type: "password", value: "x" }]);
    expect(joinRequest(joinReducer(typed, { type: "forceSlide" }))).toBeNull();
  });

  it("closes on a join and keeps the sheet with the agent's error otherwise", () => {
    const joining: JoinState = { phase: "joining", target: secured, password: "p", force: false };
    const ok = { joined: true, ip: "10.0.0.2", gateway: null, error: null };
    expect(joinReducer(joining, { type: "result", result: ok })).toBe(JOIN_CLOSED);
    const bad = { joined: false, ip: null, gateway: null, error: "wrong password" };
    expect(joinReducer(joining, { type: "result", result: bad })).toMatchObject({
      phase: "password",
      error: { text: "wrong password" },
    });
    expect(joinReducer(joining, { type: "result", result: { ...bad, error: null } })).toMatchObject({
      error: { key: "wifi.join.failed" },
    });
  });

  it("cannot be dismissed while a join is in flight", () => {
    const joining: JoinState = { phase: "joining", target: secured, password: "p", force: false };
    expect(joinReducer(joining, { type: "close" })).toBe(joining);
  });
});
