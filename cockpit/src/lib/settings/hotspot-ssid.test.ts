import { describe, expect, it } from "vitest";

import { hotspotSharesRadio, resolveHotspotSsid } from "@/shared/api/hotspot";

describe("resolveHotspotSsid", () => {
  it("names the ground station's live AP and its interface over the status name", () => {
    const out = resolveHotspotSsid(
      { hotspot_ssid: "ADOS-dfeefdc21f30" } as never,
      { ssid: "ADOS-GS-DFEE", interface: "wlan1" },
      true,
    );
    expect(out).toEqual({ known: true, ssid: "ADOS-GS-DFEE", iface: "wlan1" });
  });

  it("uses the published name on a drone, with no interface claim", () => {
    expect(resolveHotspotSsid({ hotspot_ssid: "ADOS-abc" } as never, null, false)).toEqual({
      known: true,
      ssid: "ADOS-abc",
      iface: null,
    });
    expect(resolveHotspotSsid(undefined, null, false)).toEqual({ known: false, iface: null });
  });

  it("never falls back to the drone-style name on a ground station before its AP leg arrives", () => {
    expect(resolveHotspotSsid({ hotspot_ssid: "ADOS-abc" } as never, undefined, true)).toEqual({
      known: false,
      iface: null,
    });
  });

  it("never falls back to the drone-style name while the profile is loading", () => {
    expect(resolveHotspotSsid({ hotspot_ssid: "ADOS-abc" } as never, null, null)).toEqual({
      known: false,
      iface: null,
    });
  });
});

describe("hotspotSharesRadio", () => {
  it("warns unless the AP guard counted two or more Wi-Fi radios", () => {
    expect(hotspotSharesRadio(null)).toBe(true);
    expect(hotspotSharesRadio({ wifi_phy_count: null })).toBe(true);
    expect(hotspotSharesRadio({ wifi_phy_count: 1 })).toBe(true);
    expect(hotspotSharesRadio({ wifi_phy_count: 2 })).toBe(false);
    expect(hotspotSharesRadio({ wifi_phy_count: 3 })).toBe(false);
  });
});
