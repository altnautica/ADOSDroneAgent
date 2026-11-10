// The gamepad input path: the one `getGamepads()` reader in the cockpit.
//
// Each animation frame it reads the active pad, conditions the sticks (per-pad
// calibration, radial deadzone) and publishes one sample to the input store,
// which the stick stream (`usePilotInput`) and the Settings › Controls drawing
// both read. Then `PadRouter` (`pad-router.ts`) routes the sample:
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

import { FC_RETURN_MODES } from "@/lib/fc-failsafe";
import { useEffect, useState } from "react";

import { padRoute, PadRouter } from "@/hooks/pad-router";
import { useProfile } from "@/shared/use-profile";
import { isKiosk } from "@/lib/kiosk";
import { padDisplayName, padKindOf } from "@/lib/pad/art";
import { conditionSticks } from "@/lib/pad/conditioning";
import { BUTTON_INDEX } from "@/lib/pad/ds4-map";
import type { EngineContext } from "@/lib/pad/gesture-engine";
import { pilotFeedback, playPattern, type FeedbackView } from "@/lib/pad/rumble";
import type { PilotState } from "@/shared/hud/cockpit-frames";
import { specText } from "@/shared/hud/spec-text";
import { overlayOpen } from "@/stores/cockpit-ui-store";
import { useControlsStore } from "@/stores/controls-store";
import { useInputStore, type PadSample } from "@/stores/input-store";
import { activeScreenId, useNavStore } from "@/stores/nav-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

/** How often a ground-station kiosk re-reads the station's input state. */
const GROUND_INPUT_POLL_MS = 5000;
/** Whether the flight controller's mode is a return home (ArduPilot or PX4
 *  name, any case), from the shared return-mode list. */
export function rthModeActive(mode: string | null | undefined): boolean {
  return mode != null && FC_RETURN_MODES.includes(mode.toUpperCase());
}

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
    rthActive: rthModeActive(p?.fcModeName),
    vehicleVtol: p?.vehicleClass === "vtol",
    nativeSafety: nativeInputActive(),
  };
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
    let lastPad: Gamepad | null = null;
    const router = new PadRouter(useControlsStore.getState().bindings, {
      command,
      route: () => {
        const nav = useNavStore.getState();
        return padRoute(activeScreenId(nav) === "feed", nav.quickMenuOpen, overlayOpen());
      },
      context: engineContext,
      pad: () => lastPad,
    });
    const offBindings = useControlsStore.subscribe((s, prev) => {
      if (s.bindings !== prev.bindings) router.setBindings(s.bindings);
    });

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
      if (!pad) {
        if (useInputStore.getState().pad) {
          router.lost();
          useInputStore.setState({ pad: null, holds: [], gimbalRateDps: 0, padAnnounced: false });
        }
        return;
      }
      const sample = sampleOf(pad);
      useControlsStore.getState().setPadKind(sample.kind);
      router.step(sample, performance.now());
    };

    raf = requestAnimationFrame(poll);
    return () => {
      cancelAnimationFrame(raf);
      window.removeEventListener("gamepadconnected", onConnect);
      window.removeEventListener("gamepaddisconnected", onDisconnect);
      offBindings();
      offPilot();
      router.dispose();
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

/** The controller cell's words (spec `controls.no_controller` with no pad,
 *  else `controls.pad.*`) and whether they read as good (true), bad (false)
 *  or neutral (null). */
export function padStatusText(pad: PadStatus): { text: string; ok: boolean | null } {
  if (!pad.connected) return { text: specText("controls.no_controller"), ok: null };
  const battery = pad.batteryPct !== null ? ` ${Math.round(pad.batteryPct)}%` : "";
  if (pad.native) return { text: specText("controls.pad.native", { battery }), ok: true };
  return pad.standard
    ? { text: specText("controls.pad.connected", { battery }), ok: true }
    : { text: specText("controls.pad.not_standard"), ok: false };
}
