// The gamepad input path: the one `getGamepads()` reader in the cockpit.
//
// Each animation frame it reads the active pad, conditions the sticks (per-pad
// calibration, radial deadzone) and publishes one sample to the input store,
// which the stick stream (`usePilotInput`) and the Settings › Controls drawing
// both read. Then:
// - on the Feed, the gesture engine turns the sample into the `ds4.json`
//   bindings and fires them through `runCockpitAction` (sticks fly, L2/R2
//   drive the gimbal rate, PS brakes, …), with rumble for holds, mode changes
//   and pilot-state changes;
// - elsewhere, the D-pad, Cross, Circle, Options and the left stick drive the
//   menus through the same NavCommand set the touch and panel paths use;
// - in Settings › Controls' "Press a control" mode, presses only select.
//
// Browsers expose a pad only after its first input, so `gamepadconnected`
// alone marks it announced; the Controls screen says "Press any button" until
// a sample arrives. A pad the browser does not report in the standard layout
// drives nothing: its button and axis indices are unknown, so the gesture
// engine and the stick stream ignore it and the top bar flags it.

import { useEffect, useState } from "react";

import { useProfile } from "@/shared/use-profile";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { publishGamepadButtons } from "@/lib/gamepad-bus";
import { isKiosk } from "@/lib/kiosk";
import { padDisplayName, padKindOf } from "@/lib/pad/art";
import { conditionSticks } from "@/lib/pad/conditioning";
import { BUTTON_INDEX, cockpitActionId, type ControlId, type PadBinding } from "@/lib/pad/ds4-map";
import {
  GestureEngine,
  gimbalPitchRate,
  NATIVE_ACTIONS,
  type EngineContext,
  type EngineEvent,
  type HoldProgress,
  type PadFrame,
} from "@/lib/pad/gesture-engine";
import { pilotFeedback, playPattern, playPulse, type FeedbackView } from "@/lib/pad/rumble";
import type { NavCommand } from "@/nav/navigator";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { specText } from "@/shared/hud/spec-text";
import { overlayOpen } from "@/stores/cockpit-ui-store";
import { useControlsStore } from "@/stores/controls-store";
import { useInputStore, type PadSample } from "@/stores/input-store";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

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

const AXIS_THRESHOLD = 0.6;
const AXIS_REPEAT_MS = 220;
/** How often a ground-station kiosk re-reads the station's input state. */
const GROUND_INPUT_POLL_MS = 5000;
const RTH_MODES = ["RTL", "SMART_RTL", "QRTL"];

export interface GamepadState {
  connected: boolean;
}

export interface PadStatus {
  connected: boolean;
  name: string | null;
  standard: boolean;
  batteryPct: number | null;
  /** A ground station flies the sticks natively (kiosk). */
  native: boolean;
}

/** The pad the operator is using: the last one to connect, else the first. */
function activePad(preferred: number | null): Gamepad | null {
  const pads = Array.from(navigator.getGamepads?.() ?? []).filter((p): p is Gamepad => p != null && p.connected);
  return pads.find((p) => p.index === preferred) ?? pads[0] ?? null;
}

function sampleOf(pad: Gamepad): PadSample {
  const controls = useControlsStore.getState();
  const cal = controls.calibration[pad.id] ?? null;
  const standard = pad.mapping === "standard";
  const values = pad.buttons.map((b) => b.value);
  return {
    index: pad.index,
    id: pad.id,
    name: padDisplayName(pad.id),
    kind: padKindOf(pad.id),
    standard,
    axes: [...pad.axes],
    sticks: conditionSticks(pad.axes, cal, controls.deadzone),
    pressed: pad.buttons.map((b) => b.pressed),
    values,
    l2: values[BUTTON_INDEX.l2 ?? 6] ?? 0,
    r2: values[BUTTON_INDEX.r2 ?? 7] ?? 0,
    timestamp: pad.timestamp,
    polledAt: performance.now(),
  };
}

/** A sample as the engine's frame. */
export function padFrame(s: PadSample, t: number): PadFrame {
  const pressed: Partial<Record<ControlId, boolean>> = {};
  for (const [control, i] of Object.entries(BUTTON_INDEX) as [ControlId, number][]) pressed[control] = s.pressed[i] === true;
  return { t, pressed, l2: s.l2, r2: s.r2, stickL: s.sticks.l };
}

function pilotNow(): PilotState | null {
  return useNavStreamStore.getState().pilot?.data ?? null;
}

/** The ground station flies the sticks and the safety buttons natively. On a
 *  kiosk whose input status is not known yet (first poll pending or failed)
 *  the browser assumes native input, so arm and kill never run twice. */
export function nativeInputActive(): boolean {
  const c = useControlsStore.getState();
  return c.stationKiosk && (c.groundInput === null || c.groundInput.nativeSticks === true);
}

function engineContext(): EngineContext {
  const p = pilotNow();
  return {
    pickerOpen: useInputStore.getState().pickerOpen,
    landed: p ? p.landed : null,
    rthActive: p?.fcModeName != null && RTH_MODES.includes(p.fcModeName.toUpperCase()),
    vehicleVtol: p?.vehicleClass === "vtol",
    nativeSafety: nativeInputActive(),
  };
}

function sameHolds(a: readonly HoldProgress[], b: readonly HoldProgress[]): boolean {
  return a.length === b.length && a.every((h, i) => h.action === b[i].action && Math.abs(h.progress - b[i].progress) < 0.01);
}

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

export function feedbackView(p: PilotState): FeedbackView {
  const warnings = p.readiness.filter((r) => !r.ok && r.severity === "warn").map((r) => r.id);
  if (p.holdReason) warnings.push(`hold:${p.holdReason}`);
  const criticals = p.armed ? p.readiness.filter((r) => !r.ok && r.severity === "block").map((r) => r.id) : [];
  if (p.linkLossInS !== null) criticals.push("link_loss");
  return {
    owner: p.owner,
    skillId: p.skill ? `${p.skill.pluginId}:${p.skill.skillId}` : null,
    controlMode: p.controlMode,
    armed: p.armed,
    warnings,
    criticals,
  };
}

/** On a ground station's kiosk, follow the station's input state. */
function useGroundStationInput(): void {
  const profile = useProfile();
  const station = isKiosk() && profile === "ground_station";
  useEffect(() => {
    const store = useControlsStore.getState();
    store.setStationKiosk(station);
    if (!station) return;
    const ctl = new AbortController();
    void store.loadGroundInput(ctl.signal);
    const timer = setInterval(() => void useControlsStore.getState().loadGroundInput(ctl.signal), GROUND_INPUT_POLL_MS);
    return () => {
      ctl.abort();
      clearInterval(timer);
    };
  }, [station]);
}

export function useGamepad(): GamepadState {
  const [connected, setConnected] = useState(false);
  const command = useNavStore((s) => s.command);
  useGroundStationInput();

  useEffect(() => {
    if (typeof navigator === "undefined" || !("getGamepads" in navigator)) return;
    let raf = 0;
    let preferred: number | null = null;
    let wasConnected = false;
    let prevPressed: boolean[] = [];
    let lastAxisFire = 0;
    let route: PadRoute = "nav";
    let lastPad: Gamepad | null = null;
    const engine = new GestureEngine(useControlsStore.getState().bindings);
    // Under an overlay only the safety rows run, on their own engine,
    // so no other binding fires, rumbles or draws a hold ring there.
    const safety = new GestureEngine(safetyBindings(useControlsStore.getState().bindings));
    const offBindings = useControlsStore.subscribe((s, prev) => {
      if (s.bindings === prev.bindings) return;
      engine.setBindings(s.bindings);
      safety.setBindings(safetyBindings(s.bindings));
    });

    const dispatch = (events: EngineEvent[]) => {
      for (const e of events) {
        if (e.kind === "rumble") {
          if (e.pattern === "pulse") playPulse(lastPad, e.intensity, e.ms);
          else playPattern(lastPad, e.pattern);
          continue;
        }
        const id = cockpitActionId(e.action);
        if (id) runCockpitAction(id, e.phase);
      }
    };

    const onConnect = (e: GamepadEvent) => {
      preferred = e.gamepad.index;
      useInputStore.setState({ padAnnounced: true });
    };
    const onDisconnect = (e: GamepadEvent) => {
      if (preferred === e.gamepad.index) preferred = null;
    };
    window.addEventListener("gamepadconnected", onConnect);
    window.addEventListener("gamepaddisconnected", onDisconnect);

    let feedback: FeedbackView | null = null;
    const offPilot = useNavStreamStore.subscribe((s, prev) => {
      if (s.pilot === prev.pilot || !s.pilot) return;
      const next = feedbackView(s.pilot.data);
      const pattern = pilotFeedback(feedback, next);
      feedback = next;
      if (pattern) playPattern(lastPad, pattern);
    });

    const poll = () => {
      raf = requestAnimationFrame(poll);
      const pad = activePad(preferred);
      lastPad = pad;
      if ((pad != null) !== wasConnected) {
        wasConnected = pad != null;
        setConnected(wasConnected);
      }
      const input = useInputStore.getState();
      if (!pad) {
        if (input.pad) {
          dispatch(engine.reset());
          dispatch(safety.reset());
          useInputStore.setState({ pad: null, holds: [], gimbalRateDps: 0, padAnnounced: false });
        }
        prevPressed = [];
        return;
      }

      const sample = sampleOf(pad);
      useControlsStore.getState().setPadKind(sample.kind);
      useInputStore.setState({ pad: sample, padAnnounced: true });
      const pressed = sample.pressed;
      const was = prevPressed;
      prevPressed = pressed;
      publishGamepadButtons(pressed);
      if (pressed.some((p, i) => p && !was[i])) input.setSource("gamepad");
      if (input.capture) return;

      const nav = useNavStore.getState();
      const next = padRoute(activeScreenId(nav) === "feed", nav.quickMenuOpen, overlayOpen());
      if (next !== route) {
        if (route === "flight") dispatch(engine.reset());
        if (route === "overlay") dispatch(safety.reset());
        useInputStore.setState({ holds: [], gimbalRateDps: 0 });
      }
      route = next;

      if (!sample.standard) return;
      if (route !== "nav") {
        const now = performance.now();
        const frame = padFrame(sample, now);
        const ctx = engineContext();
        const active = route === "flight" ? engine : safety;
        dispatch(active.step(frame, ctx));
        const holds = active.holds(frame, ctx);
        if (!sameHolds(holds, input.holds)) useInputStore.setState({ holds });
        if (route === "flight") {
          const rate = gimbalPitchRate(useControlsStore.getState().bindings, sample.l2, sample.r2);
          if (Math.abs(rate - input.gimbalRateDps) > 0.05) useInputStore.setState({ gimbalRateDps: rate });
          return;
        }
      }

      for (const [indexStr, cmd] of Object.entries(NAV_COMMANDS)) {
        const i = Number(indexStr);
        if (pressed[i] && !was[i]) command(cmd);
      }
      const axisY = sample.sticks.l[1];
      const now = performance.now();
      if (Math.abs(axisY) >= AXIS_THRESHOLD) {
        if (now - lastAxisFire >= AXIS_REPEAT_MS) {
          command(axisY < 0 ? "prev" : "next");
          lastAxisFire = now;
        }
      } else {
        lastAxisFire = 0;
      }
    };

    raf = requestAnimationFrame(poll);
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("gamepadconnected", onConnect);
      window.removeEventListener("gamepaddisconnected", onDisconnect);
      offBindings();
      offPilot();
      dispatch(engine.reset());
    };
  }, [command]);

  return { connected };
}

/** The controller as the top bar and the pre-flight checklist show it. */
export function usePadStatus(): PadStatus {
  const connected = useInputStore((s) => s.pad !== null);
  const name = useInputStore((s) => s.pad?.name ?? null);
  const standard = useInputStore((s) => s.pad?.standard ?? false);
  const ground = useControlsStore((s) => (s.stationKiosk ? s.groundInput : null));
  const native = ground?.nativeSticks === true;
  const station = ground?.controller ?? null;
  return {
    connected: connected || (native && station !== null),
    name: name ?? station?.name ?? null,
    standard: standard || native,
    batteryPct: station?.batteryPct ?? null,
    native,
  };
}

/** The controller cell's words (spec `controls.pad.*`) and whether they read
 *  as good (true), bad (false) or neutral (null). */
export function padStatusText(pad: PadStatus): { text: string; ok: boolean | null } {
  if (!pad.connected) return { text: specText("controls.pad.none"), ok: null };
  const battery = pad.batteryPct !== null ? ` ${Math.round(pad.batteryPct)}%` : "";
  if (pad.native) return { text: specText("controls.pad.native", { battery }), ok: true };
  return pad.standard
    ? { text: specText("controls.pad.connected", { battery }), ok: true }
    : { text: specText("controls.pad.not_standard"), ok: false };
}
