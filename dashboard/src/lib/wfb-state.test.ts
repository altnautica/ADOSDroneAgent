import { describe, expect, it } from "vitest";

import { wfbMeasured, wfbStale, wfbStateBadge } from "@/lib/wfb-state";

describe("WFB state badge", () => {
  it("treats stale as a known warning state with dimmed values", () => {
    expect(wfbStateBadge("stale")).toEqual({ label: "Stale", variant: "warning" });
    expect(wfbStale("stale")).toBe(true);
    expect(wfbStale("active")).toBe(false);
  });

  it("names the live states in words, never the wire code", () => {
    expect(wfbStateBadge("active")).toEqual({ label: "Active", variant: "success" });
    expect(wfbStateBadge("ready")?.label).toBe("Ready");
    expect(wfbStateBadge("bogus")?.label).toBe("Unrecognised");
  });

  it("claims nothing for an unknown state (agent down)", () => {
    expect(wfbStateBadge("unknown")).toBeNull();
  });
});

describe("wfbMeasured", () => {
  it("is false when nothing was decoded or the radio is deaf", () => {
    expect(wfbMeasured({ packets_received: 0 })).toBe(false);
    expect(wfbMeasured({ packets_received: 50, link_diag: "deaf" })).toBe(false);
    expect(wfbMeasured({ packets_received: 50, link_diag: "healthy" })).toBe(true);
    expect(wfbMeasured(null)).toBe(false);
  });
});

describe("receiver states", () => {
  it("names searching and a deaf radio instead of Unrecognised", () => {
    expect(wfbStateBadge("searching")).toEqual({ label: "Searching", variant: "info" });
    expect(wfbStateBadge("deaf")).toEqual({ label: "No RF from drone", variant: "warning" });
  });
});
