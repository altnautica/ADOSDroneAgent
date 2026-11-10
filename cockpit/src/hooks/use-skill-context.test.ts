import { afterEach, describe, expect, it } from "vitest";

import { skillContextNow } from "@/hooks/use-skill-context";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import { specText } from "@/shared/hud/spec-text";
import { useFlightStore } from "@/stores/flight-store";

afterEach(() => {
  useFlightStore.setState({ telemetry: null, live: false });
});

describe("the skill context's flight-controller view", () => {
  it("carries the mode and heartbeat status the telemetry reports", () => {
    useFlightStore.setState({ telemetry: { mode: "RTL", system_status: 4, armed: true }, live: true });
    expect(skillContextNow().fc).toEqual({ mode: "RTL", systemStatus: 4 });
  });

  it("lets the pause gate see a heartbeat emergency with no pilot", () => {
    useFlightStore.setState({ telemetry: { mode: "AUTO", system_status: 6, armed: true }, live: true });
    expect(resolveSkillState(BUILTIN_SKILL_BY_ID.pause!, skillContextNow())).toEqual({
      enabled: false,
      reason: specText("refusal.failsafe"),
    });
  });
});
