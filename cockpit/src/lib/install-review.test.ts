import { describe, expect, it } from "vitest";

import { canApprove, COOL_OFF_SECONDS, initialReview, requiredPermissions, reviewReducer } from "@/lib/install-review";
import {
  pinnedSource,
  removePluginPath,
  type PluginManifestSummary,
  type RiskLevel,
} from "@/shared/api/plugin-install";

function manifest(risk: RiskLevel, permRisk?: RiskLevel): PluginManifestSummary {
  return {
    ok: true,
    plugin_id: "com.example.demo",
    version: "1.0.0",
    name: "Demo",
    risk,
    signer_id: null,
    signed: false,
    halves: ["agent"],
    permissions: [
      { id: "mavlink.write", required: true, risk: permRisk },
      { id: "ui.slot.feed", required: false },
    ],
  };
}

describe("install review gate", () => {
  it("opens immediately for a low-risk plugin once on the permissions stage", () => {
    let s = initialReview(manifest("low"));
    expect(canApprove(s)).toBe(false);
    s = reviewReducer(s, { type: "continue" });
    expect(canApprove(s)).toBe(true);
  });

  it("needs the understood switch for a high-risk plugin", () => {
    let s = reviewReducer(initialReview(manifest("high")), { type: "continue" });
    expect(canApprove(s)).toBe(false);
    s = reviewReducer(s, { type: "understood", value: true });
    expect(canApprove(s)).toBe(true);
  });

  it("holds a critical grant behind the cool-off and the switch", () => {
    let s = reviewReducer(initialReview(manifest("medium", "critical")), { type: "continue" });
    s = reviewReducer(s, { type: "understood", value: true });
    expect(s.coolOff).toBe(COOL_OFF_SECONDS);
    expect(canApprove(s)).toBe(false);
    for (let i = 0; i < COOL_OFF_SECONDS; i++) s = reviewReducer(s, { type: "tick" });
    expect(canApprove(s)).toBe(true);
  });

  it("refuses to start installing while the gate is shut, and returns on failure", () => {
    const shut = reviewReducer(initialReview(manifest("critical")), { type: "continue" });
    expect(reviewReducer(shut, { type: "installing" }).stage).toBe("permissions");
    const open = reviewReducer(reviewReducer(initialReview(manifest("low")), { type: "continue" }), { type: "installing" });
    expect(open.stage).toBe("installing");
    expect(reviewReducer(open, { type: "failed" }).stage).toBe("permissions");
  });

  it("requests only the required permissions", () => {
    expect(requiredPermissions(manifest("low"))).toEqual(["mavlink.write"]);
  });
});

describe("install source and remove", () => {
  it("pins a URL install to the previewed digest, and keeps an explicit pin", () => {
    const m = { ...manifest("low"), archive_sha256: "ab".repeat(32) };
    expect(pinnedSource({ kind: "url", url: "https://x/a.adosplug", sha256: null }, m)).toEqual({
      kind: "url",
      url: "https://x/a.adosplug",
      sha256: "ab".repeat(32),
    });
    const own = { kind: "url" as const, url: "https://x/a.adosplug", sha256: "cd".repeat(32) };
    expect(pinnedSource(own, m)).toBe(own);
  });

  it("builds the remove request with keep_data", () => {
    expect(removePluginPath("com.example/x", true)).toBe("/api/plugins/com.example%2Fx?keep_data=true");
    expect(removePluginPath("a.b", false)).toBe("/api/plugins/a.b?keep_data=false");
  });
});
