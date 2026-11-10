import { describe, expect, it } from "vitest";

import { addRow, buildDoc, missingLocked, moveRow, parseControlsDoc, satisfiesLocked, setRowAction } from "@/lib/pad/bindings";
import { DEFAULT_BINDINGS, LOCKED_ACTIONS, STOP_HOLD_MS, type ControlId, type PadBinding } from "@/lib/pad/ds4-map";
import layout from "@/shared/cockpit-spec/layout.json";
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

describe("a hold row bound to the emergency stop", () => {
  const stopRow = (hold_ms: number): PadBinding => ({
    control: "triangle",
    with: [],
    gesture: "hold",
    hold_ms,
    when: null,
    action: "kill",
    rumble: "ramp",
    locked: false,
  });

  it("takes the spec stop hold", () => {
    expect(STOP_HOLD_MS).toBe(layout.holds_ms.stop);
    expect(STOP_HOLD_MS).toBe(3000);
  });

  for (const hold_ms of [1, 1500]) {
    it(`never fires kill before the stop hold (row hold ${hold_ms} ms)`, () => {
      const rows = DEFAULT_BINDINGS.filter((b) => b.control !== "triangle").concat(stopRow(hold_ms));
      const e = new GestureEngine(rows);
      const fired: string[] = [];
      for (let t = 0; t <= STOP_HOLD_MS - 100; t += 50) fired.push(...presses(e.step(frame(t, ["triangle"]), ctx)));
      expect(fired).toEqual([]);
      expect(presses(e.step(frame(STOP_HOLD_MS, ["triangle"]), ctx))).toEqual(["kill:hold"]);
    });
  }

  for (const gesture of ["press", "tap", "double_tap"] as const) {
    it(`runs a ${gesture} row bound to the stop as a stop hold`, () => {
      const row: PadBinding = { ...stopRow(0), gesture, hold_ms: null, rumble: null };
      const rows = DEFAULT_BINDINGS.filter((b) => b.control !== "triangle").concat(row);
      // A short press, released: nothing.
      const tapped = new GestureEngine(rows);
      const quick = [...presses(tapped.step(frame(0, ["triangle"]), ctx)), ...presses(tapped.step(frame(80), ctx))];
      for (let t = 100; t <= 1000; t += 50) quick.push(...presses(tapped.step(frame(t), ctx)));
      expect(quick).toEqual([]);
      // Held: nothing before the stop hold, the stop at it.
      const e = new GestureEngine(rows);
      const fired: string[] = [];
      for (let t = 0; t <= STOP_HOLD_MS - 100; t += 50) fired.push(...presses(e.step(frame(t, ["triangle"]), ctx)));
      expect(fired).toEqual([]);
      expect(presses(e.step(frame(STOP_HOLD_MS, ["triangle"]), ctx))).toEqual(["kill:hold"]);
    });
  }

  it("is raised to a stop hold by the editor whatever its gesture", () => {
    const added = addRow(DEFAULT_BINDINGS, "triangle", "press", "kill");
    expect(added[added.length - 1]).toMatchObject({ gesture: "hold", hold_ms: STOP_HOLD_MS });
  });

  it("is clamped to the stop hold by the editor", () => {
    const added = addRow(DEFAULT_BINDINGS, "triangle", "hold", "kill");
    expect(added[added.length - 1]!.hold_ms).toBe(STOP_HOLD_MS);
    const other = addRow(DEFAULT_BINDINGS, "triangle", "hold", "zoom_in");
    expect(other[other.length - 1]!.hold_ms).toBe(1500);
    const set = setRowAction(other, other.length - 1, "kill");
    expect(set.ok && set.rows[set.rows.length - 1]!.hold_ms).toBe(STOP_HOLD_MS);
  });

  it("is clamped to the stop hold on import", () => {
    const doc = buildDoc([...DEFAULT_BINDINGS, stopRow(1)], "mode2", 0.1, "ds4");
    const parsed = parseControlsDoc(JSON.stringify(doc));
    expect(parsed.ok).toBe(true);
    if (parsed.ok) expect(parsed.doc.bindings[parsed.doc.bindings.length - 1]!.hold_ms).toBe(STOP_HOLD_MS);
  });
});
