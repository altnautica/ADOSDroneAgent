import { afterEach, describe, expect, it, vi } from "vitest";

import { skillContextNow } from "@/hooks/use-skill-context";
import { BUILTIN_SKILL_BY_ID, resolveSkillState } from "@/lib/skills";
import { specText } from "@/shared/hud/spec-text";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { useStatusStore } from "@/stores/status-store";
import type { GsStatus } from "@/lib/types";

afterEach(() => {
  useFlightStore.setState({ telemetry: null, live: false });
  useNavStreamStore.setState({ pilot: null });
  useStatusStore.setState({ status: null });
  vi.useRealTimers();
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

describe("the skill context's pilot freshness", () => {
  it("is fresh under 2000 ms and stale from 2000 ms", () => {
    vi.useFakeTimers();
    vi.setSystemTime(100_000);
    const data = { armed: true, landed: false } as PilotState;
    useNavStreamStore.setState({ pilot: { data, at: 100_000 - 1999 } });
    expect(skillContextNow().pilot?.fresh).toBe(true);
    useNavStreamStore.setState({ pilot: { data, at: 100_000 - 2000 } });
    expect(skillContextNow().pilot?.fresh).toBe(false);
  });
});

describe("the skill context on a ground station in standby", () => {
  it("offers no command while a phone holds the uplink, and says who holds it", () => {
    useFlightStore.setState({ telemetry: { mode: "AUTO", system_status: 4, armed: true }, live: true });
    useStatusStore.setState({
      status: {
        fc_connected: true,
        uplink: { state: "standby", holder_device_id: "a1b2c3d4e5f60718", holder_name: "Pilot phone", since_ms: 1, watcher: { last_beacon_ms: 2 } },
      } as GsStatus,
    });
    const ctx = skillContextNow();
    expect(ctx.fcConnected).toBe(false);
    expect(resolveSkillState(BUILTIN_SKILL_BY_ID.rth!, ctx)).toEqual({ enabled: false, reason: "Uplink held by Pilot phone" });
  });

  it("commands again once the box transmits", () => {
    useFlightStore.setState({ telemetry: { mode: "AUTO", system_status: 4, armed: true }, live: true });
    useStatusStore.setState({
      status: { fc_connected: true, uplink: { state: "transmitting", watcher: { last_beacon_ms: null } } } as GsStatus,
    });
    expect(resolveSkillState(BUILTIN_SKILL_BY_ID.rth!, skillContextNow())).toEqual({ enabled: true });
  });
});
