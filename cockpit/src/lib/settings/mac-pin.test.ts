import { describe, expect, it, vi } from "vitest";

import type { MacPinResult } from "@/shared/api/mac-pin";

import { adapterAction, adapterTone, macPinOutcome, runMacPinAction } from "./mac-pin";

const result = (extra: Partial<MacPinResult> = {}): MacPinResult => ({ status: "ok", iface: "wlan0", ...extra });

describe("adapterAction", () => {
  it("offers pin on a candidate, unpin on a pinned adapter, nothing otherwise", () => {
    expect(adapterAction({ name: "wlan0", state: "candidate" })).toEqual({ kind: "pin", iface: "wlan0" });
    expect(adapterAction({ name: "wlan0", state: "pinned" })).toEqual({ kind: "unpin", iface: "wlan0" });
    expect(adapterAction({ name: "wlan0", state: "stable" })).toBeNull();
    expect(adapterAction({ name: "wlan0" })).toBeNull();
    expect(adapterAction({ name: null, state: "candidate" })).toBeNull();
    expect(adapterTone("deferred")).toBe("warn");
    expect(adapterTone("future")).toBe("muted");
  });
});

describe("macPinOutcome", () => {
  it("reports a half-failed unpin as still pinned", () => {
    expect(macPinOutcome({ kind: "unpin", iface: "wlan0" }, result({ link_error: "EACCES" }))).toEqual({
      ok: false,
      key: "settingsMore.macPin.stillPinned",
      detail: "EACCES",
    });
    expect(macPinOutcome({ kind: "unpin", iface: "wlan0" }, result({ persist_error: "refused" }))).toMatchObject({
      ok: false,
      detail: "refused",
    });
    expect(macPinOutcome({ kind: "pin", iface: "wlan0" }, result({ note: "next boot" }))).toEqual({
      ok: true,
      note: "next boot",
      key: "settingsMore.macPin.pinned",
    });
  });
});

describe("mac-pin guard", () => {
  it("building the action touches nothing; only firing it calls the agent", async () => {
    const api = { pinMac: vi.fn(async () => result()), unpinMac: vi.fn(async () => result()) };
    const action = adapterAction({ name: "wlan0", state: "candidate" });
    expect(action).not.toBeNull();
    expect(api.pinMac).not.toHaveBeenCalled();
    expect(api.unpinMac).not.toHaveBeenCalled();

    await runMacPinAction(action!, api);
    expect(api.pinMac).toHaveBeenCalledWith("wlan0");
    expect(api.unpinMac).not.toHaveBeenCalled();

    await runMacPinAction({ kind: "unpin", iface: "wlan0" }, api);
    expect(api.unpinMac).toHaveBeenCalledWith("wlan0");
  });
});
