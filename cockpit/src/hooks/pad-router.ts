// Where one pad sample goes, frame by frame: the gesture engine on the bare
// Feed, the safety rows plus the menu commands under an overlay, the menu
// commands elsewhere. `useGamepad` reads the pad and feeds each sample here;
// the tests drive the same router with synthetic samples.

import { publishGamepadButtons } from "@/lib/gamepad-bus";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { BUTTON_INDEX, cockpitActionId, TRIGGER_PRESSED, type ControlId, type PadBinding } from "@/lib/pad/ds4-map";
import {
  GestureEngine,
  gimbalPitchRate,
  NATIVE_ACTIONS,
  type EngineContext,
  type EngineEvent,
  type HoldProgress,
  type PadFrame,
} from "@/lib/pad/gesture-engine";
import { playPattern, playPulse } from "@/lib/pad/rumble";
import type { NavCommand } from "@/nav/navigator";
import { useInputStore, type PadSample } from "@/stores/input-store";

/** Menu navigation off the Feed (standard-mapping indices). */
const NAV_COMMANDS: Readonly<Record<number, NavCommand>> = {
  0: "activate",
  1: "back",
  9: "quick-menu",
  12: "prev",
  13: "next",
  14: "prev",
  15: "next",
};

/** The control at each standard-mapping button index. */
const CONTROL_AT: Readonly<Record<number, ControlId>> = Object.fromEntries(
  (Object.entries(BUTTON_INDEX) as [ControlId, number][]).map(([c, i]) => [i, c]),
);

const AXIS_THRESHOLD = 0.6;
const AXIS_REPEAT_MS = 220;

/** Where the pad goes: `flight` (every binding) on the bare Feed; `overlay`
 *  when the Menu or a sheet or dialog is open over the Feed, which the nav
 *  commands drive (Circle closes it) while the safety bindings (pause, RTH,
 *  cancel RTH, arm, kill) stay live; `nav` on every other screen. */
export type PadRoute = "flight" | "overlay" | "nav";

export function padRoute(onFeedScreen: boolean, menuOpen: boolean, overlay: boolean): PadRoute {
  if (!onFeedScreen) return "nav";
  return menuOpen || overlay ? "overlay" : "flight";
}

/** The bindings that stay live under an overlay: the safety actions (pause,
 *  RTH, cancel RTH, arm, kill) and every locked row. */
export function safetyBindings(rows: readonly PadBinding[]): PadBinding[] {
  return rows.filter((b) => b.locked || NATIVE_ACTIONS[b.action] === true);
}

/** A sample as the engine's frame. */
export function padFrame(s: PadSample, t: number): PadFrame {
  const pressed: Partial<Record<ControlId, boolean>> = {};
  for (const [control, i] of Object.entries(BUTTON_INDEX) as [ControlId, number][]) pressed[control] = s.pressed[i] === true;
  return { t, pressed, l2: s.l2, r2: s.r2, stickL: s.sticks.l };
}

function sameHolds(a: readonly HoldProgress[], b: readonly HoldProgress[]): boolean {
  return a.length === b.length && a.every((h, i) => h.action === b[i].action && Math.abs(h.progress - b[i].progress) < 0.01);
}

export interface PadRouterDeps {
  /** A menu command (activate, back, …) to the navigator. */
  command: (cmd: NavCommand) => void;
  /** The route for this frame. */
  route: () => PadRoute;
  /** The engine's view of the vehicle and the cockpit. */
  context: () => EngineContext;
  /** The pad that rumbles. */
  pad: () => Gamepad | null;
}

export class PadRouter {
  private readonly engine: GestureEngine;
  /** Under an overlay only the safety rows run, on their own engine, so no
   *  other binding fires, rumbles or draws a hold ring there. */
  private readonly safety: GestureEngine;
  private route: PadRoute = "nav";
  private was: boolean[] = [];
  private lastAxisFire = 0;
  /** Each control in a safety combo, and the other members of its combos. */
  private partners = new Map<ControlId, Set<ControlId>>();
  /** Under an overlay, a menu button that is also in a safety combo gives its
   *  command on release, and only while this says the press stayed a plain
   *  tap (no combo partner went down, no safety hold ran). */
  private cleanTap = new Map<number, boolean>();

  constructor(
    private bindings: readonly PadBinding[],
    private readonly deps: PadRouterDeps,
  ) {
    this.engine = new GestureEngine(bindings);
    this.safety = new GestureEngine(safetyBindings(bindings));
    this.indexPartners();
  }

  setBindings(bindings: readonly PadBinding[]): void {
    this.bindings = bindings;
    this.engine.setBindings(bindings);
    this.safety.setBindings(safetyBindings(bindings));
    this.indexPartners();
  }

  private indexPartners(): void {
    this.partners.clear();
    for (const b of safetyBindings(this.bindings)) {
      if (b.with.length === 0) continue;
      const members = [b.control, ...b.with];
      for (const m of members) {
        const set = this.partners.get(m) ?? new Set<ControlId>();
        for (const o of members) if (o !== m) set.add(o);
        this.partners.set(m, set);
      }
    }
  }

  private dispatch(events: EngineEvent[]): void {
    for (const e of events) {
      if (e.kind === "rumble") {
        if (e.pattern === "pulse") playPulse(this.deps.pad(), e.intensity, e.ms);
        else playPattern(this.deps.pad(), e.pattern);
        continue;
      }
      const id = cockpitActionId(e.action);
      if (id) runCockpitAction(id, e.phase);
    }
  }

  /** The pad is gone: every owed release fires. */
  lost(): void {
    this.dispatch(this.engine.reset());
    this.dispatch(this.safety.reset());
    this.was = [];
    this.cleanTap.clear();
  }

  /** The poller stopped. */
  dispose(): void {
    this.dispatch(this.engine.reset());
  }

  /** Route one sample. The sample is published to the input store here, after
   *  the capture state is read: a "Press a control" listener that ends the
   *  capture on this press must not also see it navigate. */
  step(sample: PadSample, now: number): void {
    const input = useInputStore.getState();
    useInputStore.setState({ pad: sample, padAnnounced: true });
    const pressed = sample.pressed;
    const was = this.was;
    this.was = pressed;
    publishGamepadButtons(pressed);
    if (pressed.some((p, i) => p && !was[i])) input.setSource("gamepad");
    if (input.capture) return;

    const next = this.deps.route();
    if (next !== this.route) {
      if (this.route === "flight") this.dispatch(this.engine.reset());
      if (this.route === "overlay") this.dispatch(this.safety.reset());
      useInputStore.setState({ holds: [], gimbalRateDps: 0 });
      this.cleanTap.clear();
    }
    this.route = next;

    if (!sample.standard) return;
    if (this.route === "nav") {
      for (const [indexStr, cmd] of Object.entries(NAV_COMMANDS)) {
        const i = Number(indexStr);
        if (pressed[i] && !was[i]) this.deps.command(cmd);
      }
      this.stickNav(sample, now);
      return;
    }

    const frame = padFrame(sample, now);
    const ctx = this.deps.context();
    const active = this.route === "flight" ? this.engine : this.safety;
    const events = active.step(frame, ctx);
    this.dispatch(events);
    const holds = active.holds(frame, ctx);
    if (!sameHolds(holds, input.holds)) useInputStore.setState({ holds });
    if (this.route === "flight") {
      const rate = gimbalPitchRate(this.bindings, sample.l2, sample.r2);
      if (Math.abs(rate - input.gimbalRateDps) > 0.05) useInputStore.setState({ gimbalRateDps: rate });
      return;
    }

    // Overlay: the menu commands drive it, but never out from under a safety
    // combo. While a safety hold runs, or a member of a safety combo is held,
    // no command fires: Circle in L2+R2+Circle or Options+Circle must not
    // close the sheet (or open the Menu) and restart the hold.
    const down = new Set<ControlId>((Object.keys(frame.pressed) as ControlId[]).filter((c) => frame.pressed[c]));
    if (frame.l2 >= TRIGGER_PRESSED) down.add("l2");
    if (frame.r2 >= TRIGGER_PRESSED) down.add("r2");
    const safetyBusy = holds.length > 0 || events.some((e) => e.kind === "action");
    const memberHeld = [...down].some((c) => this.partners.has(c));
    for (const [indexStr, cmd] of Object.entries(NAV_COMMANDS)) {
      const i = Number(indexStr);
      const partners = this.partners.get(CONTROL_AT[i]);
      if (!partners) {
        if (pressed[i] && !was[i] && !safetyBusy && !memberHeld) this.deps.command(cmd);
        continue;
      }
      // A combo member (Circle, Options): a plain tap gives its command on
      // release; a press that turns into a combo gives nothing.
      const blocked = safetyBusy || [...partners].some((p) => down.has(p));
      if (pressed[i] && !was[i]) this.cleanTap.set(i, !blocked);
      else if (pressed[i] && blocked) this.cleanTap.set(i, false);
      else if (!pressed[i] && was[i]) {
        if (this.cleanTap.get(i) === true) this.deps.command(cmd);
        this.cleanTap.delete(i);
      }
    }
    if (!safetyBusy && !memberHeld) this.stickNav(sample, now);
  }

  /** The left stick moves the menu focus, repeating while held. */
  private stickNav(sample: PadSample, now: number): void {
    const axisY = sample.sticks.l[1];
    if (Math.abs(axisY) >= AXIS_THRESHOLD) {
      if (now - this.lastAxisFire >= AXIS_REPEAT_MS) {
        this.deps.command(axisY < 0 ? "prev" : "next");
        this.lastAxisFire = now;
      }
    } else {
      this.lastAxisFire = 0;
    }
  }
}
