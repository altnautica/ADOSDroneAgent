// The gamepad input path for menu navigation. Reads the browser Gamepad API
// each animation frame, edge-detects the d-pad + face buttons + left-stick,
// and folds them onto the same NavCommand set the touch and button paths use
// (one dispatcher, three sources). Flight-stick control on the
// Feed screen (MANUAL_CONTROL through the PIC arbiter) is a later stage; this
// hook is UI navigation only.

import { useEffect, useRef, useState } from "react";

import { useNavStore } from "@/stores/nav-store";
import type { NavCommand } from "@/nav/navigator";

// Standard-mapping button indices (https://w3c.github.io/gamepad/#remapping).
const BTN_A = 0;
const BTN_B = 1;
const BTN_START = 9;
const BTN_DPAD_UP = 12;
const BTN_DPAD_DOWN = 13;
const BTN_DPAD_LEFT = 14;
const BTN_DPAD_RIGHT = 15;

const BUTTON_COMMANDS: Record<number, NavCommand> = {
  [BTN_A]: "activate",
  [BTN_B]: "back",
  [BTN_START]: "quick-menu",
  [BTN_DPAD_UP]: "prev",
  [BTN_DPAD_DOWN]: "next",
  [BTN_DPAD_LEFT]: "prev",
  [BTN_DPAD_RIGHT]: "next",
};

// Left-stick vertical axis deflection + re-trigger cadence for held moves.
const AXIS_THRESHOLD = 0.6;
const AXIS_REPEAT_MS = 220;

export interface GamepadState {
  connected: boolean;
}

/** Poll connected gamepads and drive the navigator for the app's lifetime. */
export function useGamepad(): GamepadState {
  const [connected, setConnected] = useState(false);
  const command = useNavStore((s) => s.command);
  const prevButtons = useRef<Record<number, boolean>>({});
  const lastAxisFire = useRef(0);

  useEffect(() => {
    if (typeof navigator === "undefined" || !("getGamepads" in navigator)) {
      return;
    }

    let raf = 0;

    const poll = () => {
      const pads = navigator.getGamepads ? navigator.getGamepads() : [];
      const pad = Array.from(pads).find((p): p is Gamepad => p != null);
      setConnected(pad != null);

      if (pad) {
        for (const [indexStr, cmd] of Object.entries(BUTTON_COMMANDS)) {
          const index = Number(indexStr);
          const pressed = pad.buttons[index]?.pressed ?? false;
          const was = prevButtons.current[index] ?? false;
          if (pressed && !was) command(cmd); // rising edge only
          prevButtons.current[index] = pressed;
        }

        // Left-stick vertical as prev/next, rate-limited so a held stick
        // repeats at a readable cadence instead of every frame.
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

      raf = requestAnimationFrame(poll);
    };

    raf = requestAnimationFrame(poll);

    return () => cancelAnimationFrame(raf);
  }, [command]);

  return { connected };
}
