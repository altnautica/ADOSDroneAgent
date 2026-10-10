// The gamepad input path. Reads the browser Gamepad API each animation frame,
// publishes the raw button sample (skill bindings and the bindings editor read
// it), and folds the d-pad, face buttons and left stick onto the NavCommand
// set the touch and panel-button paths use. While a skill confirm sheet is
// open, A (or the button that opened it) held completes the hold and B
// cancels, instead of navigating.

import { useEffect, useRef, useState } from "react";

import { publishGamepadButtons } from "@/lib/gamepad-bus";
import type { NavCommand } from "@/nav/navigator";
import { useConfirmStore } from "@/stores/confirm-store";
import { useNavStore } from "@/stores/nav-store";

// Standard-mapping button indices (https://w3c.github.io/gamepad/#remapping).
export const BTN_A = 0;
export const BTN_B = 1;
const BTN_START = 9;
const BTN_DPAD_UP = 12;
const BTN_DPAD_DOWN = 13;
const BTN_DPAD_LEFT = 14;
const BTN_DPAD_RIGHT = 15;

/** Buttons that drive menu navigation; skill bindings may not use them. */
export const NAV_BUTTONS: readonly number[] = [
  BTN_A,
  BTN_B,
  BTN_START,
  BTN_DPAD_UP,
  BTN_DPAD_DOWN,
  BTN_DPAD_LEFT,
  BTN_DPAD_RIGHT,
];

const BUTTON_COMMANDS: Record<number, NavCommand> = {
  [BTN_A]: "activate",
  [BTN_B]: "back",
  [BTN_START]: "quick-menu",
  [BTN_DPAD_UP]: "prev",
  [BTN_DPAD_DOWN]: "next",
  [BTN_DPAD_LEFT]: "prev",
  [BTN_DPAD_RIGHT]: "next",
};

const AXIS_THRESHOLD = 0.6;
const AXIS_REPEAT_MS = 220;

export interface GamepadState {
  connected: boolean;
}

export function useGamepad(): GamepadState {
  const [connected, setConnected] = useState(false);
  const command = useNavStore((s) => s.command);
  const prev = useRef<boolean[]>([]);
  const lastAxisFire = useRef(0);

  useEffect(() => {
    if (typeof navigator === "undefined" || !("getGamepads" in navigator)) return;
    let raf = 0;
    let wasConnected = false;

    const poll = () => {
      const pad = Array.from(navigator.getGamepads()).find((p): p is Gamepad => p != null);
      if ((pad != null) !== wasConnected) {
        wasConnected = pad != null;
        setConnected(wasConnected);
      }

      if (pad) {
        const pressed = pad.buttons.map((b) => b.pressed);
        const was = prev.current;
        prev.current = pressed;
        publishGamepadButtons(pressed);

        const confirm = useConfirmStore.getState();
        if (confirm.pending) {
          const holdButton = confirm.pending.gamepadButton;
          confirm.setHeld(pressed[BTN_A] || (holdButton !== null && pressed[holdButton] === true));
          if (pressed[BTN_B] && !was[BTN_B]) confirm.cancel();
        } else {
          for (const [indexStr, cmd] of Object.entries(BUTTON_COMMANDS)) {
            const i = Number(indexStr);
            if (pressed[i] && !was[i]) command(cmd);
          }
          const axisY = pad.axes[1] ?? 0;
          const now = performance.now();
          if (Math.abs(axisY) >= AXIS_THRESHOLD) {
            if (now - lastAxisFire.current >= AXIS_REPEAT_MS) {
              command(axisY < 0 ? "prev" : "next");
              lastAxisFire.current = now;
            }
          } else {
            lastAxisFire.current = 0;
          }
        }
      }
      raf = requestAnimationFrame(poll);
    };

    raf = requestAnimationFrame(poll);
    return () => cancelAnimationFrame(raf);
  }, [command]);

  return { connected };
}
