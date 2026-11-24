import { describe, expect, it } from "vitest";

import { fmtTq } from "@/shared/format";
import { activeUplinkToken, ethernetLaneView, laneForToken, laneTone, uplinkTokenLabel } from "./uplink-lanes";

describe("uplink lane tokens", () => {
  it("maps the router's interface-role tokens to their lanes", () => {
    // The router reports eth0 / wlan0_client / wwan0 / usb0. Matching lane
    // words instead meant no lane was ever marked active.
    expect(laneForToken("eth0")).toBe("ethernet");
    expect(laneForToken("wlan0_client")).toBe("wifi");
    expect(laneForToken("wwan0")).toBe("modem");
    expect(laneForToken("usb0")).toBe("usb");
    expect(uplinkTokenLabel("eth0")).toBe("Ethernet");
  });

  it("leaves an unknown token unmapped and shows it raw", () => {
    expect(laneForToken("ppp0")).toBeNull();
    expect(laneForToken(null)).toBeNull();
    expect(uplinkTokenLabel("ppp0")).toBe("ppp0");
  });
});

describe("the active uplink", () => {
  it("falls back to the status payload's uplink kind when the lane composite names none", () => {
    expect(activeUplinkToken("wlan0_client", "eth")).toBe("wlan0_client");
    expect(activeUplinkToken(null, "eth")).toBe("eth");
    expect(laneForToken(activeUplinkToken(null, "eth"))).toBe("ethernet");
    expect(laneForToken(activeUplinkToken(undefined, "wifi"))).toBe("wifi");
    expect(laneForToken(activeUplinkToken(null, "cellular"))).toBe("modem");
    expect(uplinkTokenLabel("cellular")).toBe("4G modem");
  });

  it("reads none and an absent reading as no uplink", () => {
    expect(activeUplinkToken(null, "none")).toBeNull();
    expect(activeUplinkToken(null, null)).toBeNull();
  });

  it("colours an up lane green, rings the active one, and keeps amber for a degraded active lane", () => {
    expect(laneTone({ up: true, active: false, reachable: true })).toEqual({ tone: "ok", ring: false });
    expect(laneTone({ up: true, active: true, reachable: true })).toEqual({ tone: "ok", ring: true });
    expect(laneTone({ up: null, active: true, reachable: null })).toEqual({ tone: "ok", ring: true });
    expect(laneTone({ up: true, active: true, reachable: false })).toEqual({ tone: "warn", ring: true });
    expect(laneTone({ up: false, active: false, reachable: false })).toEqual({ tone: "muted", ring: false });
    expect(laneTone({ up: null, active: false, reachable: true })).toEqual({ tone: "muted", ring: false });
  });
});

describe("ethernet lane", () => {
  it("renders the live wired port the API reports", () => {
    expect(ethernetLaneView({ link: true, ip: "10.0.0.2", speed_mbps: 1000 })).toEqual({
      up: true,
      detail: "10.0.0.2 · 1000 Mb/s",
    });
    expect(ethernetLaneView(null)).toEqual({ up: null });
  });

  it("maps the sentinel's kernel names to lanes", () => {
    expect(laneForToken("end0")).toBe("ethernet");
    expect(laneForToken("wlan1")).toBe("wifi");
  });
});

describe("fmtTq", () => {
  it("scales every batman TQ by 255, including values at or below 100", () => {
    expect(fmtTq(255)).toBe("100%");
    expect(fmtTq(90)).toBe("35%");
    expect(fmtTq(null)).toBe("—");
  });
});
