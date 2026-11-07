import { describe, expect, it } from "vitest";

import { missingLocked, moveRow, satisfiesLocked } from "@/lib/pad/bindings";
import { DEFAULT_BINDINGS, LOCKED_ACTIONS, type ControlId, type PadBinding } from "@/lib/pad/ds4-map";
import { GestureEngine, type EngineContext, type EngineEvent, type PadFrame } from "@/lib/pad/gesture-engine";

const ctx: EngineContext = { pickerOpen: false, landed: false, rthActive: false, vehicleVtol: false, nativeSafety: false };

function frame(t: number, down: ControlId[] = []): PadFrame {
  const pressed: Partial<Record<ControlId, boolean>> = {};
  for (const c of down) pressed[c] = true;
  return { t, pressed, l2: 0, r2: 0, stickL: [0, 0] };
}

function presses(events: EngineEvent[]): string[] {
  return events.flatMap((e) => (e.kind === "action" && e.phase !== "release" ? [`${e.action}:${e.phase}`] : []));
}

describe("PS brake and RTH hold", () => {
  it("fires pause the moment PS goes down, then RTH at 2 s without a release", () => {
    const e = new GestureEngine(DEFAULT_BINDINGS);
    expect(presses(e.step(frame(0, ["ps"]), ctx))).toEqual(["pause:press"]);
    expect(presses(e.step(frame(1500, ["ps"]), ctx))).toEqual([]);
    expect(presses(e.step(frame(2000, ["ps"]), ctx))).toEqual(["rth:hold"]);
    expect(presses(e.step(frame(2100), ctx))).toEqual([]);
  });

  it("a short PS press brakes once and nothing fires on release", () => {
    const e = new GestureEngine(DEFAULT_BINDINGS);
    expect(presses(e.step(frame(0, ["ps"]), ctx))).toEqual(["pause:press"]);
    expect(presses(e.step(frame(120), ctx))).toEqual([]);
  });

  it("cancels RTH on press while RTH flies", () => {
    const e = new GestureEngine(DEFAULT_BINDINGS);
    expect(presses(e.step(frame(0, ["ps"]), { ...ctx, rthActive: true }))).toEqual(["rth_cancel:press"]);
  });
});

describe("Options + Circle fallback for a swallowed PS", () => {
  it("brakes on press and reaches RTH after a 2 s hold", () => {
    const e = new GestureEngine(DEFAULT_BINDINGS);
    e.step(frame(0, ["options"]), ctx);
    expect(presses(e.step(frame(10, ["options", "circle"]), ctx))).toEqual(["pause:press"]);
    expect(presses(e.step(frame(2010, ["options", "circle"]), ctx))).toEqual(["rth:hold"]);
  });
});

describe("locked bindings", () => {
  const rows = (): PadBinding[] => DEFAULT_BINDINGS.map((b) => ({ ...b, with: [...b.with] }));

  it("the defaults satisfy every locked action", () => {
    expect(missingLocked(rows())).toEqual([]);
  });

  it("refuses moving a locked row onto a stick or the lightbar", () => {
    const r = rows();
    const i = r.findIndex((b) => b.locked && b.action === "rth");
    expect(moveRow(r, i, "stick_l").ok).toBe(false);
    expect(moveRow(r, i, "lightbar").ok).toBe(false);
    expect(moveRow(r, i, "triangle").ok).toBe(true);
  });

  it("an import that shortens the arm hold or drops its centred gate is missing arm", () => {
    const arm = LOCKED_ACTIONS.find((l) => l.action === "arm")!;
    const def = DEFAULT_BINDINGS.find((b) => b.locked && b.action === "arm")!;
    expect(satisfiesLocked(def, arm)).toBe(true);
    expect(satisfiesLocked({ ...def, hold_ms: 50 }, arm)).toBe(false);
    expect(satisfiesLocked({ ...def, when: null }, arm)).toBe(false);
    expect(satisfiesLocked({ ...def, control: "stick_r" }, arm)).toBe(false);
  });
});
