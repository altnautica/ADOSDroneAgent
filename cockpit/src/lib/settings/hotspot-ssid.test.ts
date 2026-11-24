import { describe, expect, it } from "vitest";

import { resolveHotspotSsid } from "@/shared/api/hotspot";

describe("resolveHotspotSsid", () => {
  it("names the ground station's live AP and its interface over the status name", () => {
    const out = resolveHotspotSsid(
      { hotspot_ssid: "ADOS-dfeefdc21f30" } as never,
      { ssid: "ADOS-GS-DFEE", interface: "wlan1" },
    );
    expect(out).toEqual({ known: true, ssid: "ADOS-GS-DFEE", iface: "wlan1" });
  });

  it("falls back to the published name with no interface claim", () => {
    expect(resolveHotspotSsid({ hotspot_ssid: "ADOS-abc" } as never)).toEqual({
      known: true,
      ssid: "ADOS-abc",
      iface: null,
    });
    expect(resolveHotspotSsid(undefined, null)).toEqual({ known: false, iface: null });
  });
});
