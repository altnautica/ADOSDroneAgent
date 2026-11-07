// Which input the operator used last, and the controller's live sample.
//
// Controller hints (the hint bar) show only while a gamepad or the panel's own
// buttons drive the cockpit; a touch, a click or a key hides them again.
//
// `pad` is the one input sample the single gamepad poller (`useGamepad`)
// publishes each animation frame: raw and conditioned axes, buttons, analog
// triggers and the pad's own timestamp. The stick stream and the Settings ›
// Controls live drawing both read it; nothing else calls `getGamepads()`.

import { useEffect } from "react";
import { create } from "zustand";

import type { PadKind } from "@/lib/pad/bindings";
import type { Sticks } from "@/lib/pad/conditioning";
import type { HoldProgress } from "@/lib/pad/gesture-engine";

export type InputSource = "pointer" | "keyboard" | "gamepad" | "panel";

export interface PadSample {
  /** `Gamepad.index`. */
  index: number;
  /** `Gamepad.id`: the calibration key. */
  id: string;
  /** The id without the browser's mapping/vendor suffix. */
  name: string;
  kind: PadKind;
  /** The browser reports the W3C standard layout. */
  standard: boolean;
  axes: number[];
  sticks: Sticks;
  pressed: boolean[];
  values: number[];
  l2: number;
  r2: number;
  /** `Gamepad.timestamp`: when the pad last reported a change. Chromium only
   *  advances it when a value changes, so a stick held still freezes it. */
  timestamp: number;
  /** When the poller last read this pad while it was connected
   *  (`performance.now()`): the stick stream's liveness clock. */
  polledAt: number;
}

interface InputState {
  source: InputSource;
  setSource: (source: InputSource) => void;
  /** The live controller sample, or null with no pad reporting. */
  pad: PadSample | null;
  /** A pad announced itself (`gamepadconnected`) but has not reported yet:
   *  browsers expose a pad only after its first input. */
  padAnnounced: boolean;
  /** "Press a control" in Settings › Controls: presses select a control
   *  instead of navigating or firing actions. */
  capture: boolean;
  setCapture: (on: boolean) => void;
  /** The skill picker sheet is open (D-pad ↑/↓ adjusts a parameter rather
   *  than the layer level). Set by the picker. */
  pickerOpen: boolean;
  setPickerOpen: (open: boolean) => void;
  /** L2/R2 gimbal pitch rate, °/s, + up. Read by the gimbal driver. */
  gimbalRateDps: number;
  /** Controller holds in progress (arm combo, PS → RTH, Triangle). */
  holds: HoldProgress[];
}

export const useInputStore = create<InputState>((set, get) => ({
  source: "pointer",
  setSource: (source) => {
    if (get().source !== source) set({ source });
  },
  pad: null,
  padAnnounced: false,
  capture: false,
  setCapture: (capture) => set({ capture }),
  pickerOpen: false,
  setPickerOpen: (pickerOpen) => set({ pickerOpen }),
  gimbalRateDps: 0,
  holds: [],
}));

/** Whether a controller (gamepad or panel buttons) was the last input. */
export function isControllerSource(source: InputSource): boolean {
  return source === "gamepad" || source === "panel";
}

/** Track pointer and keyboard input for the app's lifetime. The gamepad and
 *  the panel buttons report themselves from their own input paths. */
export function useInputSourceTracking(): void {
  useEffect(() => {
    const setSource = useInputStore.getState().setSource;
    const onPointer = () => setSource("pointer");
    const onKey = () => setSource("keyboard");
    window.addEventListener("pointerdown", onPointer, { capture: true, passive: true });
    window.addEventListener("keydown", onKey, { capture: true, passive: true });
    return () => {
      window.removeEventListener("pointerdown", onPointer, { capture: true });
      window.removeEventListener("keydown", onKey, { capture: true });
    };
  }, []);
}
