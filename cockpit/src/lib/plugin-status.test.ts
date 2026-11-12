import { describe, expect, it } from "vitest";

import { installStatusKey, installStatusTone, parseInstalls } from "@/lib/plugin-catalog";

describe("extension install status", () => {
  it("maps every host status to a label key, never the raw word", () => {
    for (const s of ["installed", "enabled", "running", "disabled", "failed", "incompatible"]) {
      expect(installStatusKey(s)).toBe(`extensions.status.${s}`);
    }
    expect(installStatusKey("WEIRD")).toBe("extensions.status.unknown");
    expect(installStatusKey(null)).toBe("extensions.status.unknown");
  });

  it("tones a failed start as an error, unlike a disabled one", () => {
    expect(installStatusTone("failed")).toBe("err");
    expect(installStatusTone("incompatible")).toBe("warn");
    expect(installStatusTone("disabled")).toBe("muted");
  });

  it("carries the failure reason", () => {
    const [row] = parseInstalls({ installs: [{ plugin_id: "a", status: "failed", failure_reason: "crashed on start" }] });
    expect(row.failureReason).toBe("crashed on start");
  });
});
