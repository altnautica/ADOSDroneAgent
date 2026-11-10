import { describe, expect, it, vi } from "vitest";

vi.mock("@/shared/hud/spec-text", () => ({
  specText: (key: string, args: Record<string, unknown> = {}) => `<${key}${JSON.stringify(args)}>`,
}));

import { shellBanners } from "@/lib/shell-banners";
import strings from "@/shared/cockpit-spec/setup-strings.json";

describe("agent-offline banner text", () => {
  it("comes from the spec string", () => {
    const [b] = shellBanners({ statusStale: true, refusal: "none", profile: "drone", fcConnected: true, hasStatus: true });
    expect(b.message).toBe("<shell.agent_offline{}>");
    expect((strings as { strings: Record<string, string> }).strings["shell.agent_offline"]).toBeTruthy();
  });
});
