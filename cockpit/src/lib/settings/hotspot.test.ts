import { describe, expect, it } from "vitest";

import { hotspotToggle, wifiClientConnected } from "@/lib/settings/hotspot";

describe("hotspotToggle", () => {
  it("holds enabling behind the slide while a Wi-Fi client is connected", () => {
    expect(hotspotToggle(true, true)).toEqual({ kind: "confirm" });
  });

  it("commits enabling with no client link, and disabling always", () => {
    expect(hotspotToggle(true, false)).toEqual({ kind: "commit", enabled: true });
    expect(hotspotToggle(false, true)).toEqual({ kind: "commit", enabled: false });
    expect(hotspotToggle(false, false)).toEqual({ kind: "commit", enabled: false });
  });
});

describe("wifiClientConnected", () => {
  it("counts only an explicit connected status", () => {
    expect(wifiClientConnected(null)).toBe(false);
    expect(wifiClientConnected(undefined)).toBe(false);
    const base = { ssid: "x", bssid: "", signal: null, ip: null, gateway: null, security: null };
    expect(wifiClientConnected({ ...base, connected: false })).toBe(false);
    expect(wifiClientConnected({ ...base, connected: true })).toBe(true);
  });
});
