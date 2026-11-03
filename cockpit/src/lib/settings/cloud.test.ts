import { describe, expect, it, vi } from "vitest";

import { openGate, settleGate } from "@/lib/settings/apply-form";
import { cloudDirty, cloudModeFrom, cloudSeed, tunnelTokenBody, validateCloud } from "@/lib/settings/cloud";
import type { AgentConfig } from "@/shared/api/types";

const config = {
  server: { self_hosted: { url: "https://convex.example.com", mqtt_broker: "mqtt.example.com", mqtt_port: 1883 } },
} as unknown as AgentConfig;

describe("cloud seed", () => {
  it("reads an unknown posture as local and seeds endpoints from config", () => {
    expect(cloudModeFrom(undefined)).toBe("local");
    expect(cloudModeFrom("self_hosted")).toBe("self_hosted");
    expect(cloudSeed("cloud", config)).toEqual({
      mode: "cloud",
      url: "https://convex.example.com",
      broker: "mqtt.example.com",
      port: "1883",
      apiKey: "",
    });
    expect(cloudSeed("local", null).port).toBe("8883");
  });
});

describe("cloud draft", () => {
  const initial = cloudSeed("self_hosted", config);

  it("counts endpoint edits only while self-hosted", () => {
    expect(cloudDirty(initial, initial)).toBe(false);
    expect(cloudDirty({ ...initial, apiKey: "k" }, initial)).toBe(true);
    expect(cloudDirty({ ...initial, port: "8883" }, initial)).toBe(true);
    const local = cloudSeed("local", config);
    expect(cloudDirty({ ...local, broker: "other" }, local)).toBe(false);
    expect(cloudDirty({ ...local, mode: "cloud" }, local)).toBe(true);
  });

  it("validates the self-hosted endpoints and drops an empty api key", () => {
    expect(validateCloud(initial)).toEqual({
      ok: true,
      section: {
        mode: "self_hosted",
        self_hosted: {
          url: "https://convex.example.com",
          mqtt_broker: "mqtt.example.com",
          mqtt_port: 1883,
          api_key: undefined,
        },
      },
    });
    const bad = validateCloud({ ...initial, url: "not a url" });
    expect(bad.ok).toBe(false);
    if (!bad.ok) expect(bad.error.startsWith("self_hosted.url:")).toBe(true);
    expect(validateCloud({ ...initial, port: "70000" }).ok).toBe(false);
    expect(validateCloud({ ...initial, mode: "local", url: "" })).toEqual({ ok: true, section: { mode: "local" } });
  });

  it("applies nothing until the slide confirms", () => {
    const post = vi.fn();
    const v = validateCloud({ ...initial, mode: "cloud" });
    if (!v.ok) throw new Error("expected valid");
    const gate = openGate(v.section);
    const dismissed = settleGate(gate, false);
    if (dismissed) post(dismissed);
    expect(post).not.toHaveBeenCalled();
    const slid = settleGate(gate, true);
    if (slid) post(slid);
    expect(post).toHaveBeenCalledWith({ mode: "cloud" });
  });
});

describe("tunnelTokenBody", () => {
  it("trims the token and refuses an empty one, so the slide stays disabled", () => {
    expect(tunnelTokenBody("   ")).toBeNull();
    expect(tunnelTokenBody(" abc ")).toEqual({ token_or_script: "abc" });
  });
});
