// The controller gesture engine: turns per-frame control states into binding
// firings (tap / hold / double-tap, single controls and combos) per the
// `ds4.json` row semantics. Pure and clock-driven, so the poller and the tests
// step it the same way.
//
// Rules:
// - A combo row (`with` non-empty) is the set {control} ∪ with; it starts the
//   moment the last member goes down, in any order. Starting a combo consumes
//   every member, so their own single-control rows do not also fire.
// - A control whose rows are all taps and that is in no combo fires `press`
//   on the way down and `release` on the way up (immediate).
// - Otherwise its tap fires on release: at once, or after the double-tap
//   window when a double-tap row applies, so a hold (Triangle 1.5 s, PS 2 s)
//   and a combo (L1+R1) never also fire the tap.
// - A `press` row fires (press, then release) the moment its control or combo
//   goes down, and the episode keeps timing: a hold row on the same control
//   still fires on top (PS: brake at once, RTH after 2 s held).
// - A hold fires its `hold` phase once it has been held `hold_ms` and its
//   `release` on the way up. A row bound to the emergency stop is always a
//   hold of at least the spec stop hold (`stopHoldRow`): a press or tap row
//   bound to it runs as that hold. `stick_l_centered` restarts the hold
//   whenever the left stick leaves the centre.
// - `when` gates a row at the moment it would fire.

import {
  CENTERED_THRESHOLD,
  DOUBLE_TAP_MS,
  effectiveHoldMs,
  stopHoldRow,
  RUMBLE,
  TRIGGER_PRESSED,
  type ControlId,
  type PadBinding,
  type WhenCondition,
} from "@/lib/pad/ds4-map";
import type { Stick } from "@/lib/pad/conditioning";

export interface PadFrame {
  /** Monotonic ms. */
  t: number;
  /** Digital state per button control (triggers also report their value). */
  pressed: Partial<Record<ControlId, boolean>>;
  /** L2 / R2 analog values, 0..1. */
  l2: number;
  r2: number;
  /** The conditioned left stick (for `stick_l_centered`). */
  stickL: Stick;
}

/** The actions a ground station's native input fires itself. */
export const NATIVE_ACTIONS: Readonly<Record<string, true>> = {
  pause: true,
  rth: true,
  rth_cancel: true,
  arm: true,
  kill: true,
};

export interface EngineContext {
  pickerOpen: boolean;
  /** `pilot.landed`, or null when unknown (treated as on the ground). */
  landed: boolean | null;
  rthActive: boolean;
  vehicleVtol: boolean;
  /** A ground station's native input owns the safety actions (pause, RTH,
   *  cancel RTH, arm, kill): the browser ignores every row firing one. */
  nativeSafety: boolean;
}

export type EnginePhase = "press" | "hold" | "release";

export type EngineEvent =
  | { kind: "action"; action: string; phase: EnginePhase; binding: PadBinding }
  | { kind: "rumble"; pattern: "mode_change" | "warning" | "critical" }
  | { kind: "rumble"; pattern: "pulse"; intensity: number; ms: number };

export interface HoldProgress {
  action: string;
  controls: ControlId[];
  progress: number;
}

interface Episode {
  start: number;
  /** The row that fired and still owes a `release`. */
  fired: PadBinding | null;
  /** The hold fired; nothing else fires until release. */
  done: boolean;
  lastPulse: number;
}

interface ComboGroup {
  key: string;
  members: ControlId[];
  rows: PadBinding[];
}

/** Stop rows already reported as raised to the stop hold, so each is logged
 *  once. */
const reportedStopRows = new Set<string>();

/** The row as it runs, logging once when an emergency-stop row is raised to
 *  the stop hold. */
function asRun(b: PadBinding): PadBinding {
  const run = stopHoldRow(b);
  const key = `${[b.control, ...b.with].join("+")}:${b.gesture}:${b.hold_ms ?? ""}`;
  if (run !== b && !reportedStopRows.has(key)) {
    reportedStopRows.add(key);
    console.warn(`controller: emergency-stop binding ${key} runs as a ${run.hold_ms} ms hold`);
  }
  return run;
}

const isTrigger = (c: ControlId) => c === "l2" || c === "r2";

export function whenSatisfied(when: WhenCondition | null, ctx: EngineContext, frame: PadFrame): boolean {
  switch (when) {
    case null:
      return true;
    case "picker_open":
      return ctx.pickerOpen;
    case "picker_closed":
      return !ctx.pickerOpen;
    case "on_ground":
      return ctx.landed !== false;
    case "airborne":
      return ctx.landed === false;
    case "rth_active":
      return ctx.rthActive;
    case "not_rth":
      return !ctx.rthActive;
    case "stick_l_centered":
      return Math.abs(frame.stickL[0]) < CENTERED_THRESHOLD && Math.abs(frame.stickL[1]) < CENTERED_THRESHOLD;
    case "vehicle_vtol":
      return ctx.vehicleVtol;
  }
}

export function comboKey(members: readonly ControlId[]): string {
  return [...members].sort().join("+");
}

export class GestureEngine {
  private rows: PadBinding[] = [];
  private singles = new Map<ControlId, PadBinding[]>();
  private combos: ComboGroup[] = [];
  private comboMembers = new Set<ControlId>();
  private nativeSafety = false;

  private down = new Set<ControlId>();
  private singleEp = new Map<ControlId, Episode>();
  private comboEp = new Map<string, Episode>();
  private consumed = new Set<ControlId>();
  private pendingTap = new Map<ControlId, number>();
  private source: readonly PadBinding[];

  constructor(bindings: readonly PadBinding[]) {
    this.source = bindings;
    this.index(false);
  }

  setBindings(bindings: readonly PadBinding[]): void {
    this.source = bindings;
    this.index(this.nativeSafety);
  }

  private index(nativeSafety: boolean): void {
    this.nativeSafety = nativeSafety;
    this.rows = this.source
      .filter((b) => b.gesture !== "analog" && b.action !== "none" && !(nativeSafety && NATIVE_ACTIONS[b.action]))
      .map(asRun);
    this.singles.clear();
    const groups = new Map<string, ComboGroup>();
    this.comboMembers.clear();
    for (const b of this.rows) {
      if (b.with.length === 0) {
        const list = this.singles.get(b.control) ?? [];
        list.push(b);
        this.singles.set(b.control, list);
        continue;
      }
      const members = [b.control, ...b.with];
      const key = comboKey(members);
      const g = groups.get(key) ?? { key, members, rows: [] };
      g.rows.push(b);
      groups.set(key, g);
      for (const m of members) this.comboMembers.add(m);
    }
    this.combos = [...groups.values()];
  }

  /** The holds in progress, for the on-screen ring and the editor. */
  holds(frame: PadFrame, ctx: EngineContext): HoldProgress[] {
    const out: HoldProgress[] = [];
    const add = (rows: PadBinding[], ep: Episode, controls: ControlId[]) => {
      if (ep.done) return;
      const hold = this.pickHold(rows, ctx, frame);
      const ms = hold ? effectiveHoldMs(hold) : null;
      if (!hold || ms == null) return;
      out.push({ action: hold.action, controls, progress: Math.min(1, (frame.t - ep.start) / ms) });
    };
    for (const [c, ep] of this.singleEp) add(this.singles.get(c) ?? [], ep, [c]);
    for (const [key, ep] of this.comboEp) {
      const g = this.combos.find((x) => x.key === key);
      if (g) add(g.rows, ep, g.members);
    }
    return out;
  }

  /** Release everything (pad lost, screen left): owed `release`s fire. */
  reset(): EngineEvent[] {
    const out: EngineEvent[] = [];
    for (const ep of [...this.singleEp.values(), ...this.comboEp.values()]) {
      if (ep.fired) out.push({ kind: "action", action: ep.fired.action, phase: "release", binding: ep.fired });
    }
    this.singleEp.clear();
    this.comboEp.clear();
    this.consumed.clear();
    this.pendingTap.clear();
    this.down.clear();
    return out;
  }

  step(frame: PadFrame, ctx: EngineContext): EngineEvent[] {
    if (ctx.nativeSafety !== this.nativeSafety) this.index(ctx.nativeSafety);
    const out: EngineEvent[] = [];
    const now = new Set<ControlId>();
    for (const [c, on] of Object.entries(frame.pressed) as [ControlId, boolean][]) {
      if (on && !isTrigger(c)) now.add(c);
    }
    if (frame.l2 >= TRIGGER_PRESSED) now.add("l2");
    if (frame.r2 >= TRIGGER_PRESSED) now.add("r2");

    const falling = [...this.down].filter((c) => !now.has(c));
    const rising = [...now].filter((c) => !this.down.has(c));
    this.down = now;

    for (const c of falling) this.onUp(c, frame, ctx, out);
    for (const c of rising) this.onDown(c, frame, ctx, out);
    this.advanceHolds(frame, ctx, out);

    for (const [c, deadline] of this.pendingTap) {
      if (frame.t < deadline) continue;
      this.pendingTap.delete(c);
      this.fireTap(this.applicable(this.singles.get(c) ?? [], "tap", ctx, frame), out);
    }
    return out;
  }

  private applicable(rows: readonly PadBinding[], gesture: PadBinding["gesture"], ctx: EngineContext, frame: PadFrame) {
    return rows.find((b) => b.gesture === gesture && whenSatisfied(b.when, ctx, frame)) ?? null;
  }

  private pickHold(rows: readonly PadBinding[], ctx: EngineContext, frame: PadFrame): PadBinding | null {
    let best: PadBinding | null = null;
    for (const b of rows) {
      if (b.gesture !== "hold" || b.hold_ms == null) continue;
      // `stick_l_centered` restarts the hold rather than hiding it.
      if (b.when !== "stick_l_centered" && !whenSatisfied(b.when, ctx, frame)) continue;
      if (!best || effectiveHoldMs(b)! < (effectiveHoldMs(best) ?? Infinity)) best = b;
    }
    return best;
  }

  private fire(b: PadBinding, phase: EnginePhase, out: EngineEvent[]): void {
    out.push({ kind: "action", action: b.action, phase, binding: b });
    if (phase !== "release" && b.rumble === "mode_change") out.push({ kind: "rumble", pattern: "mode_change" });
  }

  private fireTap(b: PadBinding | null, out: EngineEvent[]): void {
    if (!b) return;
    this.fire(b, "press", out);
    this.fire(b, "release", out);
  }

  private onDown(c: ControlId, frame: PadFrame, ctx: EngineContext, out: EngineEvent[]): void {
    // Combos first: every group this control completes, largest set wins.
    const completed = this.combos.filter(
      (g) => g.members.includes(c) && g.members.every((m) => this.down.has(m)) && !this.comboEp.has(g.key),
    );
    if (completed.length > 0) {
      const size = Math.max(...completed.map((g) => g.members.length));
      for (const g of completed.filter((x) => x.members.length === size)) {
        for (const m of g.members) {
          this.consumed.add(m);
          const ep = this.singleEp.get(m);
          if (ep?.fired) this.fire(ep.fired, "release", out);
          this.singleEp.delete(m);
          this.pendingTap.delete(m);
        }
        const ep: Episode = { start: frame.t, fired: null, done: false, lastPulse: frame.t };
        this.comboEp.set(g.key, ep);
        this.fireTap(this.applicable(g.rows, "press", ctx, frame), out);
        const hasHold = g.rows.some((b) => b.gesture === "hold");
        if (!hasHold) {
          const tap = this.applicable(g.rows, "tap", ctx, frame);
          if (tap) {
            this.fire(tap, "press", out);
            ep.fired = tap;
            ep.done = true;
          }
        }
      }
      return;
    }
    if (this.consumed.has(c)) return;
    const rows = this.singles.get(c);
    if (!rows || rows.length === 0) return;
    const ep: Episode = { start: frame.t, fired: null, done: false, lastPulse: frame.t };
    this.singleEp.set(c, ep);
    this.fireTap(this.applicable(rows, "press", ctx, frame), out);
    const immediate = !this.comboMembers.has(c) && rows.every((b) => b.gesture === "tap");
    if (immediate) {
      const tap = this.applicable(rows, "tap", ctx, frame);
      if (tap) {
        this.fire(tap, "press", out);
        ep.fired = tap;
      }
      ep.done = true;
    }
  }

  private onUp(c: ControlId, frame: PadFrame, ctx: EngineContext, out: EngineEvent[]): void {
    for (const [key, ep] of this.comboEp) {
      const g = this.combos.find((x) => x.key === key);
      if (!g || !g.members.includes(c)) continue;
      this.comboEp.delete(key);
      if (ep.fired) this.fire(ep.fired, "release", out);
      else if (!ep.done) this.fireTap(this.applicable(g.rows, "tap", ctx, frame), out);
    }
    if (this.consumed.delete(c)) {
      this.singleEp.delete(c);
      return;
    }
    const ep = this.singleEp.get(c);
    if (!ep) return;
    this.singleEp.delete(c);
    if (ep.fired) {
      this.fire(ep.fired, "release", out);
      return;
    }
    if (ep.done) return;
    const rows = this.singles.get(c) ?? [];
    const double = this.applicable(rows, "double_tap", ctx, frame);
    if (!double) {
      this.fireTap(this.applicable(rows, "tap", ctx, frame), out);
      return;
    }
    if (this.pendingTap.has(c)) {
      this.pendingTap.delete(c);
      this.fireTap(double, out);
    } else {
      this.pendingTap.set(c, frame.t + DOUBLE_TAP_MS);
    }
  }

  private advanceHolds(frame: PadFrame, ctx: EngineContext, out: EngineEvent[]): void {
    const advance = (rows: readonly PadBinding[], ep: Episode) => {
      if (ep.done) return;
      const hold = this.pickHold(rows, ctx, frame);
      const ms = hold ? effectiveHoldMs(hold) : null;
      if (!hold || ms == null) return;
      if (hold.when === "stick_l_centered" && !whenSatisfied(hold.when, ctx, frame)) {
        ep.start = frame.t;
        ep.lastPulse = frame.t;
        return;
      }
      const elapsed = frame.t - ep.start;
      if (elapsed >= ms) {
        this.fire(hold, "hold", out);
        ep.fired = hold;
        ep.done = true;
        return;
      }
      const progress = elapsed / ms;
      if (hold.rumble === "ramp" && frame.t - ep.lastPulse >= RUMBLE.ramp.tick_ms) {
        ep.lastPulse = frame.t;
        const intensity = RUMBLE.ramp.from + (RUMBLE.ramp.to - RUMBLE.ramp.from) * progress;
        out.push({ kind: "rumble", pattern: "pulse", intensity, ms: RUMBLE.ramp.tick_ms });
      } else if (hold.rumble === "countdown" && frame.t - ep.lastPulse >= RUMBLE.countdown.every_ms) {
        ep.lastPulse = frame.t;
        out.push({ kind: "rumble", pattern: "pulse", intensity: 1, ms: RUMBLE.countdown.pulse_ms });
      }
    };
    for (const [c, ep] of this.singleEp) advance(this.singles.get(c) ?? [], ep);
    for (const [key, ep] of this.comboEp) {
      const g = this.combos.find((x) => x.key === key);
      if (g) advance(g.rows, ep);
    }
  }
}

/** The analog rows' rates: L2/R2 drive the gimbal pitch rate, full travel =
 *  the row's `rate_full_dps`. Positive = pitch up. */
export function gimbalPitchRate(bindings: readonly PadBinding[], l2: number, r2: number): number {
  let rate = 0;
  for (const b of bindings) {
    if (b.gesture !== "analog") continue;
    const v = b.control === "l2" ? l2 : b.control === "r2" ? r2 : 0;
    const full = b.rate_full_dps ?? 0;
    if (b.action === "gimbal_pitch_up") rate += v * full;
    else if (b.action === "gimbal_pitch_down") rate -= v * full;
  }
  return rate;
}
