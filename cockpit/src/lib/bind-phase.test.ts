import { describe, expect, it } from "vitest";

import { bindOutcome, bindView } from "@/lib/bind-phase";

describe("the bind session view", () => {
  it("labels each supervisor step through a key, with its age", () => {
    expect(bindView({ state: "waiting_peer", phase_age_s: 12.4 })).toEqual({
      phaseKey: "cockpit.pair.bindPhase.waiting_peer",
      ageS: 12,
      error: null,
    });
  });

  it("carries the failure reason", () => {
    expect(bindView({ state: "failed", error: "no peer heard" })?.error).toBe("no peer heard");
  });

  it("shows nothing for no session or an idle one, and unknown for an unlisted step", () => {
    expect(bindView(null)).toBeNull();
    expect(bindView({})).toBeNull();
    expect(bindView({ state: "idle" })).toBeNull();
    expect(bindView({ state: "warp" })?.phaseKey).toBe("cockpit.pair.bindPhase.unknown");
  });

  it("reads the POST reply's terminal state as the result", () => {
    expect(bindOutcome({ state: "paired" })).toEqual({ kind: "paired", reason: null });
    expect(bindOutcome({ state: "failed", error: "timeout" })).toEqual({ kind: "failed", reason: "timeout" });
    expect(bindOutcome({ state: "aborted" })).toEqual({ kind: "aborted", reason: null });
    expect(bindOutcome({})).toBeNull();
  });
});
