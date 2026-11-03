import { describe, expect, it } from "vitest";

import {
  boardOverrideFrom,
  canApplyOverride,
  logLevelChange,
  logLevelFrom,
  overrideRequest,
} from "@/lib/settings/advanced";
import type { AgentConfig } from "@/shared/api/types";

const cfg = (c: unknown) => c as AgentConfig;

describe("log level", () => {
  it("reads the configured level case-insensitively and falls back to info", () => {
    expect(logLevelFrom(cfg({ logging: { level: "WARNING" } }))).toBe("warning");
    expect(logLevelFrom(cfg({ logging: { level: "verbose" } }))).toBe("info");
    expect(logLevelFrom(null)).toBe("info");
  });

  it("sends nothing for a pick of the current level", () => {
    expect(logLevelChange("info", "info")).toBeNull();
    expect(logLevelChange("debug", "info")).toEqual({ log_level: "debug" });
  });
});

describe("board override", () => {
  it("reads the override, empty when unset", () => {
    expect(boardOverrideFrom(cfg({ agent: { board_override: "rpi4" } }))).toBe("rpi4");
    expect(boardOverrideFrom(cfg({}))).toBe("");
  });

  it("accepts a slug or an empty clear, refuses anything else", () => {
    expect(overrideRequest("rock-5c_lite")).toEqual({ ok: true, body: { board_override: "rock-5c_lite" } });
    expect(overrideRequest("")).toEqual({ ok: true, body: { board_override: "" } });
    expect(overrideRequest("bad slug").ok).toBe(false);
    expect(overrideRequest("x".repeat(65)).ok).toBe(false);
  });

  it("keeps the guard disabled for an unchanged, invalid or in-flight override", () => {
    expect(canApplyOverride("rpi4", "rpi4", false)).toBe(false);
    expect(canApplyOverride("bad slug", "", false)).toBe(false);
    expect(canApplyOverride("rpi5", "rpi4", true)).toBe(false);
    expect(canApplyOverride("rpi5", "rpi4", false)).toBe(true);
    expect(canApplyOverride("", "rpi4", false)).toBe(true);
  });
});
