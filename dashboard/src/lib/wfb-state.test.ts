import { describe, expect, it } from "vitest";

import { wfbStale, wfbStateBadge } from "@/lib/wfb-state";

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
