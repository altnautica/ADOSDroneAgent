import { afterEach, describe, expect, it, vi } from "vitest";

import { takeBackUplink, uplinkHeldReason, uplinkView } from "./uplink-handover";

const STANDBY = {
  state: "standby",
  holder_device_id: "a1b2c3d4e5f60718",
  holder_name: "Pilot phone",
  since_ms: 1_700_000_000_000,
  watcher: { last_beacon_ms: 1_700_000_005_000 },
};

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("uplinkView", () => {
  it("reads a standby block with its holder and times", () => {
    expect(uplinkView(STANDBY)).toEqual({
      state: "standby",
      holder: "Pilot phone",
      sinceMs: 1_700_000_000_000,
      lastBeaconMs: 1_700_000_005_000,
    });
  });

  it("names the holder by device id when it sent no name", () => {
    expect(uplinkView({ ...STANDBY, holder_name: null })?.holder).toBe("a1b2c3d4e5f60718");
  });

  it("reads transmitting with no holder", () => {
    const v = uplinkView({ state: "transmitting", holder_device_id: null, holder_name: null, since_ms: 5, watcher: { last_beacon_ms: null } });
    expect(v).toEqual({ state: "transmitting", holder: null, sinceMs: 5, lastBeaconMs: null });
  });

  it("is null for a missing block or an unknown state, never a guess", () => {
    expect(uplinkView(null)).toBeNull();
    expect(uplinkView(undefined)).toBeNull();
    expect(uplinkView({ state: "linked" })).toBeNull();
  });
});

describe("uplinkHeldReason", () => {
  it("refuses with the holder only while in standby", () => {
    expect(uplinkHeldReason(uplinkView(STANDBY))).toBe("Uplink held by Pilot phone");
    expect(uplinkHeldReason(uplinkView({ state: "transmitting" }))).toBeNull();
    expect(uplinkHeldReason(null)).toBeNull();
  });
});

describe("takeBackUplink", () => {
  it("posts the take-back op to the uplink route", async () => {
    const fetchMock = vi.fn(async () => new Response(JSON.stringify({ ok: true, state: "transmitting" }), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);
    await expect(takeBackUplink()).resolves.toEqual({ ok: true, state: "transmitting" });
    const [path, init] = fetchMock.mock.calls[0] as unknown as [string, RequestInit];
    expect(path).toBe("/api/v1/ground-station/uplink");
    expect(init.method).toBe("POST");
    expect(JSON.parse(String(init.body))).toEqual({ op: "take_back" });
  });
});
