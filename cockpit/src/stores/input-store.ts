// Which input the operator used last. Controller hints (the hint bar) show
// only while a gamepad or the panel's own buttons drive the cockpit; a touch,
// a click or a key hides them again.

import { useEffect } from "react";
import { create } from "zustand";

export type InputSource = "pointer" | "keyboard" | "gamepad" | "panel";

interface InputState {
  source: InputSource;
  setSource: (source: InputSource) => void;
}

export const useInputStore = create<InputState>((set, get) => ({
  source: "pointer",
  setSource: (source) => {
    if (get().source !== source) set({ source });
  },
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
