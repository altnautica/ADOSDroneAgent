import { describe, expect, it } from "vitest";

import type { AgentConfig } from "@/hooks/use-config";

import { modeFromConfig, regionFromConfig } from "./region";

describe("region posture from config", () => {
  it("reads a pinned region from network.regulatory", () => {
    const cfg: AgentConfig = { network: { regulatory: { mode: "region", region: "de" } } };
    expect(modeFromConfig(cfg.network?.regulatory)).toBe("region");
    expect(regionFromConfig(cfg.network?.regulatory)).toBe("DE");
  });

  it("treats an absent block or unrestricted mode as unrestricted", () => {
    expect(modeFromConfig(undefined)).toBe("unrestricted");
    expect(regionFromConfig(undefined)).toBeNull();
    expect(regionFromConfig({ mode: "unrestricted", region: "US" })).toBeNull();
  });
});
