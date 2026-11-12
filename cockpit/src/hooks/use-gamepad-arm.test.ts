// An unknown arm or landed state (the pilot's flight-controller link is down)
// is possibly airborne for the controller: its feedback reads armed, its
// on-ground gestures stay off, and the tips neither show nor mark one seen.

import { afterEach, describe, expect, it } from "vitest";

import { engineContext, feedbackView } from "@/hooks/use-gamepad";
import { tipsArmState, tourAirborne } from "@/lib/controller-tour";
import { whenSatisfied } from "@/lib/pad/gesture-engine";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

const PILOT = { owner: "idle", controlMode: "ados", readiness: [{ id: "fc_profile", ok: false, severity: "block", msg: "" }], holdReason: null, linkLossInS: null, skill: null } as unknown as PilotState;

afterEach(() => {
  useNavStreamStore.getState().clear();
  useFlightStore.setState({ telemetry: null, live: false, stale: false });
});

describe("an unknown arm state on the controller", () => {
  it("reads as armed in the pad's feedback, with the blocking items critical", () => {
    const v = feedbackView({ ...PILOT, armed: null, landed: null });
    expect(v.armed).toBe(true);
    expect(v.criticals).toContain("fc_profile");
    expect(feedbackView({ ...PILOT, armed: false, landed: true }).armed).toBe(false);
  });

  it("keeps the on-ground gestures off while landed is unknown", () => {
    useNavStreamStore.setState({ pilot: { data: { ...PILOT, armed: null, landed: null }, at: Date.now() } });
    const ctx = engineContext();
    expect(ctx.landed).toBe(false);
    expect(whenSatisfied("on_ground", ctx, {} as never)).toBe(false);
    expect(whenSatisfied("airborne", ctx, {} as never)).toBe(true);
    useNavStreamStore.setState({ pilot: { data: { ...PILOT, armed: false, landed: true }, at: Date.now() } });
    expect(whenSatisfied("on_ground", engineContext(), {} as never)).toBe(true);
  });
});

describe("the controller's gestures with no fresh pilot", () => {
  const onGround = () => whenSatisfied("on_ground", engineContext(), {} as never);
  const airborne = () => whenSatisfied("airborne", engineContext(), {} as never);

  it("counts an unknown arm state as airborne, so Triangle-hold lands rather than takes off", () => {
    expect(onGround()).toBe(false);
    expect(airborne()).toBe(true);
    useFlightStore.setState({ telemetry: { armed: true } });
    expect(onGround()).toBe(false);
  });

  it("reads on the ground from live telemetry reporting disarmed", () => {
    useFlightStore.setState({ telemetry: { armed: false }, live: true, stale: false });
    expect(onGround()).toBe(true);
    expect(airborne()).toBe(false);
    useFlightStore.setState({ stale: true });
    expect(onGround()).toBe(false);
  });

  it("ignores a stale pilot frame's landed state", () => {
    useNavStreamStore.setState({ pilot: { data: { ...PILOT, armed: false, landed: true }, at: Date.now() - 10_000 } });
    useFlightStore.setState({ telemetry: { armed: true } });
    expect(onGround()).toBe(false);
    expect(airborne()).toBe(true);
  });
});

describe("an unknown arm state for the tips and the tour", () => {
  it("passes the unknown through, so no tip shows and none is marked seen", () => {
    expect(tipsArmState({ armed: null, landed: null }, false)).toEqual({ armed: null, airborne: null });
    expect(tipsArmState({ armed: false, landed: true }, true)).toEqual({ armed: false, airborne: false });
    expect(tipsArmState({ armed: true, landed: false }, null)).toEqual({ armed: true, airborne: true });
    expect(tipsArmState(null, undefined)).toEqual({ armed: false, airborne: true });
  });

  it("counts as airborne for the tour", () => {
    expect(tourAirborne({ armed: null, landed: null }, false)).toBe(true);
  });
});
