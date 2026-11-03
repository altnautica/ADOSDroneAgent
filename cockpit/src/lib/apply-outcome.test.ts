import { describe, expect, it } from "vitest";

import { readApplyResponse } from "@/lib/apply-outcome";

describe("apply outcome", () => {
  it("reads a clean apply", () => {
    const o = readApplyResponse({ overall: true, sections: { network: { ok: true, message: "ok" } }, rolled_back: [] });
    expect(o.ok).toBe(true);
    expect(o.restartRequired).toBe(false);
    expect(o.summary).toBe("Applied.");
  });

  it("flags a section that needs a restart", () => {
    const o = readApplyResponse({
      overall: true,
      sections: { regulatory: { ok: true, message: "ok", data: { restart_required: true } } },
      rolled_back: [],
    });
    expect(o.restartRequired).toBe(true);
  });

  it("reports the failing section and what was rolled back", () => {
    const o = readApplyResponse({
      overall: false,
      sections: { profile: { ok: true, message: "ok" }, network: { ok: false, message: "bad ssid" } },
      rolled_back: ["profile"],
    });
    expect(o.ok).toBe(false);
    expect(o.failed).toEqual([{ section: "network", message: "bad ssid" }]);
    expect(o.rolledBack).toEqual(["profile"]);
    expect(o.summary).toContain("network: bad ssid");
    expect(o.summary).toContain("rolled back profile");
  });

  it("never reads a missing or unconfirmed reply as success", () => {
    expect(readApplyResponse(null).ok).toBe(false);
    expect(readApplyResponse({ overall: false, sections: {}, rolled_back: [] }).ok).toBe(false);
  });
});
