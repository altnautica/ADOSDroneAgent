// A button the UI already saw go down always gets its release, whatever the
// router starts masking in between: the menu commands under a safety combo,
// or a capture (the controller tour, "Press a control"). A hold that started
// before the mask never completes on time that ran while it was masked.

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { PadRouter, type PadRoute } from "@/hooks/pad-router";
import { registerCockpitActions, type ActionPhase } from "@/lib/cockpit-actions";
import { onGamepadButtons } from "@/lib/gamepad-bus";
import { BUTTON_INDEX, DEFAULT_BINDINGS, type ControlId, type PadBinding } from "@/lib/pad/ds4-map";
import type { EngineContext } from "@/lib/pad/gesture-engine";
import { useInputStore, type PadSample } from "@/stores/input-store";

const ctx: EngineContext = { pickerOpen: false, landed: true, rthActive: false, vehicleVtol: false, nativeSafety: false };

function sample(down: ControlId[]): PadSample {
  const pressed = Array.from({ length: 17 }, () => false);
  for (const c of down) pressed[BUTTON_INDEX[c]!] = true;
  return {
    index: 0,
    id: "pad",
    name: "pad",
    kind: "ds4",
    standard: true,
    axes: [0, 0, 0, 0],
    sticks: { l: [0, 0], r: [0, 0] },
    pressed,
    values: pressed.map((p) => (p ? 1 : 0)),
    l2: down.includes("l2") ? 1 : 0,
    r2: down.includes("r2") ? 1 : 0,
    timestamp: 0,
    polledAt: 0,
  };
}

function row(control: ControlId, gesture: PadBinding["gesture"], action: string, holdMs: number | null = null): PadBinding {
  return { control, with: [], gesture, hold_ms: holdMs, when: null, action, rumble: null, locked: false };
}

let route: PadRoute = "flight";
let clock = 0;
let router: PadRouter;
let phases: string[] = [];
const offs: (() => void)[] = [];

function makeRouter(bindings: readonly PadBinding[]): void {
  router = new PadRouter(bindings, { command: () => {}, route: () => route, context: () => ctx, pad: () => null });
}

/** Step every 50 ms from `from` to `to` with `down` held. */
function hold(down: ControlId[], from: number, to: number): void {
  for (let t = from; t <= to; t += 50) {
    clock = t;
    router.step(sample(down), t);
  }
}

beforeEach(() => {
  route = "flight";
  clock = 0;
  phases = [];
  useInputStore.setState({ capture: false, holds: [], pad: null });
  offs.push(
    registerCockpitActions(
      ["test_hold", "test_tap"].map((id) => ({
        id,
        label: id,
        group: "flight" as const,
        run: (phase: ActionPhase) => phases.push(`${id}:${phase}`),
      })),
    ),
  );
});

afterEach(() => {
  for (const off of offs.splice(0)) off();
});

describe("a hold button driven by Cross, under a sheet", () => {
  /** A 1.5 s hold on Cross, as the hold hook reads it from the button bus. */
  function holdButton(): { confirmed: () => boolean; holding: () => boolean } {
    let since: number | null = null;
    let confirmed = false;
    offs.push(
      onGamepadButtons((pressed, prev) => {
        if (pressed[0] && !prev[0]) since = clock;
        else if (!pressed[0] && prev[0]) since = null;
        if (since !== null && clock - since >= 1500) confirmed = true;
      }),
    );
    return { confirmed: () => confirmed, holding: () => since !== null };
  }

  it("is cancelled when Cross comes up after Circle went down", () => {
    makeRouter(DEFAULT_BINDINGS);
    route = "overlay";
    const button = holdButton();
    hold(["cross"], 0, 300);
    hold(["cross", "circle"], 350, 600);
    hold(["circle"], 650, 2500);
    expect(button.holding()).toBe(false);
    expect(button.confirmed()).toBe(false);
  });

  it("is cancelled when Cross comes up while a safety combo is held", () => {
    makeRouter(DEFAULT_BINDINGS);
    route = "overlay";
    const button = holdButton();
    hold(["cross"], 0, 300);
    hold(["cross", "l2", "r2"], 350, 600);
    hold(["l2", "r2"], 650, 2500);
    expect(button.holding()).toBe(false);
    expect(button.confirmed()).toBe(false);
  });
});

describe("a capture starting while a binding is held", () => {
  it("delivers the owed release as the capture starts", () => {
    makeRouter([row("cross", "tap", "test_tap")]);
    hold(["cross"], 0, 100);
    expect(phases).toEqual(["test_tap:press"]);
    useInputStore.setState({ capture: true });
    hold(["cross"], 150, 150);
    expect(phases).toEqual(["test_tap:press", "test_tap:release"]);
    hold([], 200, 300);
    useInputStore.setState({ capture: false });
    hold([], 350, 400);
    expect(phases).toEqual(["test_tap:press", "test_tap:release"]);
  });

  it("never completes a hold on time spent captured, nor on the press held through it", () => {
    makeRouter([row("triangle", "hold", "test_hold", 1500)]);
    hold(["triangle"], 0, 1000);
    useInputStore.setState({ capture: true });
    hold(["triangle"], 1050, 1300);
    useInputStore.setState({ capture: false });
    hold(["triangle"], 1350, 3500);
    hold([], 3550, 3600);
    expect(phases).toEqual([]);
    // A fresh press after the capture holds as usual.
    hold(["triangle"], 3650, 5200);
    expect(phases).toEqual(["test_hold:hold"]);
  });
});
