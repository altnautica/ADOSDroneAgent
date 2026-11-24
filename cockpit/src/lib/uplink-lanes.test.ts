import { describe, expect, it } from "vitest";

import { fmtTq } from "@/shared/format";
import { ethernetLaneView, laneForToken, uplinkTokenLabel } from "./uplink-lanes";

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
