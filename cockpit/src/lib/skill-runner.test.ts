import { describe, expect, it } from "vitest";

import { bypassesBusy } from "@/lib/skill-runner";
import { BUILTIN_SKILL_BY_ID } from "@/lib/skills";

describe("busy gate", () => {
  it("lets pause and kill through while another command is in flight", () => {
    expect(bypassesBusy(BUILTIN_SKILL_BY_ID.pause!)).toBe(true);
    expect(bypassesBusy(BUILTIN_SKILL_BY_ID.kill!)).toBe(true);
  });

  it("queues every other built-in behind the pending command", () => {
    for (const id of ["arm", "disarm", "takeoff", "land", "rth", "resume", "take_control"]) {
      expect(bypassesBusy(BUILTIN_SKILL_BY_ID[id]!)).toBe(false);
    }
  });
});
