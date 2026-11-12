// @vitest-environment happy-dom
// Take off or Land: the left column's `useOnGround`, rendered against the
// live stores, with the pilot frame's freshness and an unknown arm state.

import { createElement } from "react";
import { act } from "react-dom/test-utils";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, describe, expect, it } from "vitest";

import type { PilotState } from "@/shared/hud/cockpit-frames";
import type { VehicleState } from "@/lib/types";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

import { groundFrom, useOnGround } from "./on-ground";

Object.assign(globalThis, { IS_REACT_ACT_ENVIRONMENT: true });

let root: Root | null = null;
let host: HTMLDivElement | null = null;

async function ground(pilot: { ageMs: number; landed: boolean | null; armed: boolean | null } | null, telemetryArmed: boolean | undefined): Promise<boolean> {
  useNavStreamStore.setState({
    pilot: pilot ? { data: { landed: pilot.landed, armed: pilot.armed } as PilotState, at: Date.now() - pilot.ageMs } : null,
  });
  useFlightStore.setState({ telemetry: telemetryArmed === undefined ? null : ({ armed: telemetryArmed } as VehicleState) });
  let seen: boolean | null = null;
  const Probe = () => {
    seen = useOnGround();
    return null;
  };
  host = document.createElement("div");
  root = createRoot(host);
  await act(async () => root!.render(createElement(Probe)));
  return seen!;
}

afterEach(async () => {
  if (root) await act(async () => root!.unmount());
  host?.remove();
  root = null;
  useNavStreamStore.setState({ pilot: null });
  useFlightStore.setState({ telemetry: null });
});

describe("useOnGround", () => {
  it("reads a fresh pilot's landed", async () => {
    expect(await ground({ ageMs: 200, landed: true, armed: true }, true)).toBe(true);
  });

  it("reads a fresh pilot in the air as airborne", async () => {
    expect(await ground({ ageMs: 200, landed: false, armed: true }, true)).toBe(false);
  });

  it("ignores a 2.5 s old landed frame and falls back to the armed telemetry", async () => {
    expect(await ground({ ageMs: 2500, landed: true, armed: false }, true)).toBe(false);
  });

  it("reads disarmed telemetry as on the ground with no pilot", async () => {
    expect(await ground(null, false)).toBe(true);
  });

  it("reads an unknown telemetry arm state (FC link down) as possibly airborne", async () => {
    expect(await ground(null, undefined)).toBe(false);
  });

  it("reads a fresh pilot that has not heard the flight controller as possibly airborne: Land, not Take off", async () => {
    expect(await ground({ ageMs: 200, landed: null, armed: null }, false)).toBe(false);
  });
});

describe("groundFrom", () => {
  it("is on the ground only on a reported landed or disarmed", () => {
    expect(groundFrom({ landed: true, armed: null }, undefined)).toBe(true);
    expect(groundFrom({ landed: null, armed: false }, undefined)).toBe(true);
    expect(groundFrom({ landed: null, armed: null }, false)).toBe(false);
    expect(groundFrom({ landed: null, armed: true }, false)).toBe(false);
  });
});
