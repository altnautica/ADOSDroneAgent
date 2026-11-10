import { describe, expect, it } from "vitest";

import { hasSpecText, specText } from "./spec-text";

describe("spec text", () => {
  it("fills args and resolves enumerated values through their own keys", () => {
    expect(specText("flow.link_loss.countdown", { action: "rth", in_s: 5 })).toBe("Link lost: Return home in 5 s");
    expect(specText("pilot.banner.fc_control", { mode: "loiter" })).toMatch(/LOITER/);
  });

  it("fills a missing arg as empty and an unknown key as itself", () => {
    expect(specText("refusal.unknown")).toBe("Refused: ");
    expect(specText("no.such.key")).toBe("no.such.key");
    expect(hasSpecText("no.such.key")).toBe(false);
    expect(hasSpecText("refusal.none")).toBe(true);
  });
});
