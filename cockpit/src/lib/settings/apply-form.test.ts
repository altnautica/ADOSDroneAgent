import { describe, expect, it } from "vitest";

import { CLOSED_GATE, openGate, sectionVerdict, settleGate } from "@/lib/settings/apply-form";
import type { ApplyResponse } from "@/shared/api/apply-actions";

describe("confirm gate", () => {
  it("fires nothing while closed, on cancel, or before the slide", () => {
    expect(settleGate(CLOSED_GATE, true)).toBeNull();
    expect(settleGate(openGate({ mode: "local" }), false)).toBeNull();
  });

  it("fires exactly the payload snapshotted when the confirm opened", () => {
    const draft = { mode: "region", region: "DE" };
    const gate = openGate({ ...draft });
    draft.region = "US";
    expect(settleGate(gate, true)).toEqual({ mode: "region", region: "DE" });
  });
});

describe("sectionVerdict", () => {
  const reply = (overall: boolean, ok: boolean, extra: Partial<ApplyResponse["sections"][string]> = {}): ApplyResponse => ({
    overall,
    sections: { cloud: { ok, message: "", ...extra } },
    rolled_back: [],
  });

  it("needs both overall and the section ok", () => {
    expect(sectionVerdict(reply(true, true), "cloud").ok).toBe(true);
    expect(sectionVerdict(reply(false, true), "cloud").ok).toBe(false);
    expect(sectionVerdict(reply(true, false), "cloud").ok).toBe(false);
    expect(sectionVerdict(reply(true, true), "profile").ok).toBe(false);
    expect(sectionVerdict(null, "cloud").ok).toBe(false);
  });

  it("passes the agent's message through and treats an empty one as absent", () => {
    expect(sectionVerdict(reply(true, false, { message: "busy" }), "cloud").message).toBe("busy");
    expect(sectionVerdict(reply(true, true), "cloud").message).toBeNull();
  });

  it("reports restart_required only on success", () => {
    const data = { restart_required: true };
    expect(sectionVerdict(reply(true, true, { data }), "cloud").restartRequired).toBe(true);
    expect(sectionVerdict(reply(false, true, { data }), "cloud").restartRequired).toBe(false);
  });
});
