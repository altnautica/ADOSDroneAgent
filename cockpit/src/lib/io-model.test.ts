import { describe, expect, it } from "vitest";

import { canMakePrimary, pairOutcome, parseBtDevices, parseDisplay, parseGamepads, unpairedFound } from "@/lib/io-model";

describe("IO reads", () => {
  it("reads the controllers and the primary, dropping entries without an id", () => {
    const pads = parseGamepads({ devices: [{ device_id: "a", name: "Pad" }, { name: "no id" }], primary_id: "a" });
    expect(pads).toEqual({ devices: [{ deviceId: "a", name: "Pad", path: null }], primaryId: "a" });
    expect(parseGamepads(null)).toEqual({ devices: [], primaryId: null });
  });

  it("reads the display config", () => {
    expect(parseDisplay({ resolution: "1920x1080", kiosk_enabled: true })).toEqual({
      resolution: "1920x1080",
      kioskEnabled: true,
      kioskTargetUrl: null,
    });
    expect(parseDisplay("x")).toBeNull();
  });

  it("lists only unpaired scan results, comparing MACs case-insensitively", () => {
    const paired = parseBtDevices({ devices: [{ mac: "AA:BB:CC:00:11:22", connected: true }] });
    const found = parseBtDevices({ devices: [{ mac: "aa:bb:cc:00:11:22" }, { mac: "DD:EE:FF:00:11:22", name: "Pad" }] });
    expect(unpairedFound(found, paired).map((d) => d.mac)).toEqual(["DD:EE:FF:00:11:22"]);
  });

  it("reads a pair result", () => {
    expect(pairOutcome({ paired: true }).outcome).toBe("connected");
    expect(pairOutcome({ paired: true, connected: false }).outcome).toBe("pairedNotConnected");
    expect(pairOutcome({ paired: false, error: "timeout" })).toEqual({ outcome: "failed", error: "timeout" });
  });
});

describe("primary controller gate", () => {
  const pads = { devices: [{ deviceId: "a", name: null, path: null }, { deviceId: "b", name: null, path: null }], primaryId: "a" };
  it("offers the guarded write only for an attached, non-primary controller while idle", () => {
    expect(canMakePrimary(pads, "b", false)).toBe(true);
    expect(canMakePrimary(pads, "a", false)).toBe(false);
    expect(canMakePrimary(pads, "c", false)).toBe(false);
    expect(canMakePrimary(pads, "b", true)).toBe(false);
  });
});
