// With the Menu or a sheet open over the Feed, the pad drives the overlay
// (Circle closes it, never stops a skill) and only the safety bindings stay
// live underneath.

import { describe, expect, it } from "vitest";

import { padRoute, safetyBindings } from "@/hooks/use-gamepad";
import { DEFAULT_BINDINGS, type ControlId } from "@/lib/pad/ds4-map";
import { GestureEngine, type EngineContext, type EngineEvent, type PadFrame } from "@/lib/pad/gesture-engine";

const ctx: EngineContext = { pickerOpen: false, landed: false, rthActive: false, vehicleVtol: false, nativeSafety: false };

function frame(t: number, down: ControlId[] = []): PadFrame {
  const pressed: Partial<Record<ControlId, boolean>> = {};
  for (const c of down) pressed[c] = true;
  return { t, pressed, l2: down.includes("l2") ? 1 : 0, r2: down.includes("r2") ? 1 : 0, stickL: [0, 0] };
}

const fired = (events: EngineEvent[]) =>
  events.flatMap((e) => (e.kind === "action" && e.phase !== "release" ? [`${e.action}:${e.phase}`] : []));

describe("pad route", () => {
  it("hands the pad to an open overlay or Menu on the Feed", () => {
    expect(padRoute(true, false, false)).toBe("flight");
    expect(padRoute(true, false, true)).toBe("overlay");
    expect(padRoute(true, true, false)).toBe("overlay");
    expect(padRoute(false, false, true)).toBe("nav");
  });

  it("keeps only the safety bindings live under an overlay", () => {
    const actions = new Set(safetyBindings(DEFAULT_BINDINGS).map((b) => b.action));
    expect(actions).toEqual(new Set(["pause", "rth", "rth_cancel", "arm", "kill"]));
  });

  it("Circle under an overlay fires no skill stop, while PS still brakes and the kill hold still fires", () => {
    const e = new GestureEngine(safetyBindings(DEFAULT_BINDINGS));
    expect(fired(e.step(frame(0, ["circle"]), ctx))).toEqual([]);
    expect(fired(e.step(frame(100), ctx))).toEqual([]);
    expect(fired(e.step(frame(200, ["ps"]), ctx))).toEqual(["pause:press"]);
    e.step(frame(300), ctx);
    e.step(frame(400, ["l2", "r2"]), ctx);
    e.step(frame(410, ["l2", "r2", "circle"]), ctx);
    expect(fired(e.step(frame(3410, ["l2", "r2", "circle"]), ctx))).toContain("kill:hold");
  });
});
