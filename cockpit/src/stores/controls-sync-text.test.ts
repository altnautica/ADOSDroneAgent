import { describe, expect, it, vi } from "vitest";

vi.mock("@/shared/hud/spec-text", () => ({
  specText: (key: string, args: Record<string, unknown> = {}) => `<${key}${JSON.stringify(args)}>`,
}));

import { syncRefusedText } from "@/stores/controls-store";

describe("controller sync refusal", () => {
  it("names the missing actions through the spec string", () => {
    expect(syncRefusedText(["arm", "rth"])).toBe('<controls.sync.refused{"actions":"arm, rth"}>');
  });
});
