// A timed press-and-hold on one control, from every input path: a pointer
// held on it, Enter or Space held while it has focus, the gamepad's A held
// while it has focus, and a long press of the panel's select button (which
// completes the hold in one gesture). The control draws `progress` (0..1);
// `onComplete` runs once when the hold fills.

import { useCallback, useEffect, useRef, useState, type KeyboardEvent, type PointerEvent, type RefObject } from "react";

import { onGamepadButtons } from "@/lib/gamepad-bus";
import { claimPanelHold } from "@/lib/hold-input";

/** The gamepad's A in the standard mapping. */
const GAMEPAD_HOLD_BUTTON = 0;

export interface HoldOptions {
  enabled: boolean;
  holdMs: number;
  onComplete: () => void;
  /** The focusable control the hold belongs to. */
  targetRef: RefObject<HTMLElement | null>;
}

export interface Hold {
  progress: number;
  holding: boolean;
  pointerHandlers: {
    onPointerDown: (e: PointerEvent<HTMLElement>) => void;
    onPointerUp: () => void;
    onPointerCancel: () => void;
    onPointerLeave: () => void;
  };
  keyHandlers: {
    onKeyDown: (e: KeyboardEvent<HTMLElement>) => void;
    onKeyUp: (e: KeyboardEvent<HTMLElement>) => void;
  };
}

/** The keys that hold a focused control. */
const HOLD_KEYS = ["Enter", " "];

export function useHold({ enabled, holdMs, onComplete, targetRef }: HoldOptions): Hold {
  const [since, setSince] = useState<number | null>(null);
  const [progress, setProgress] = useState(0);
  const completeRef = useRef(onComplete);
  useEffect(() => {
    completeRef.current = onComplete;
  });

  const start = useCallback(() => {
    if (enabled) setSince((s) => s ?? performance.now());
  }, [enabled]);
  const stop = useCallback(() => {
    setSince(null);
    setProgress(0);
  }, []);

  // The fill, animated only while something holds.
  useEffect(() => {
    if (since === null || !enabled) return;
    let raf = 0;
    const tick = () => {
      const p = Math.min(1, (performance.now() - since) / holdMs);
      if (p >= 1) {
        setSince(null);
        setProgress(0);
        completeRef.current();
        return;
      }
      setProgress(p);
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [since, holdMs, enabled]);

  // The gamepad's A, held while this control has focus.
  useEffect(() => {
    if (!enabled) return;
    return onGamepadButtons((pressed, prev) => {
      if (document.activeElement !== targetRef.current) return;
      if (pressed[GAMEPAD_HOLD_BUTTON] && !prev[GAMEPAD_HOLD_BUTTON]) start();
      else if (!pressed[GAMEPAD_HOLD_BUTTON] && prev[GAMEPAD_HOLD_BUTTON]) stop();
    });
  }, [enabled, start, stop, targetRef]);

  // A panel long-press completes the hold while this control has focus.
  useEffect(() => {
    const el = targetRef.current;
    if (!enabled || !el) return;
    let release: (() => void) | null = null;
    const onFocus = () => {
      release = claimPanelHold(() => completeRef.current());
    };
    const onBlur = () => {
      release?.();
      release = null;
    };
    el.addEventListener("focus", onFocus);
    el.addEventListener("blur", onBlur);
    if (document.activeElement === el) onFocus();
    return () => {
      el.removeEventListener("focus", onFocus);
      el.removeEventListener("blur", onBlur);
      onBlur();
    };
  }, [enabled, targetRef]);

  return {
    progress: enabled ? progress : 0,
    holding: enabled && since !== null,
    pointerHandlers: {
      onPointerDown: (e) => {
        e.currentTarget.setPointerCapture?.(e.pointerId);
        start();
      },
      onPointerUp: stop,
      onPointerCancel: stop,
      onPointerLeave: stop,
    },
    keyHandlers: {
      onKeyDown: (e) => {
        if (!enabled || !HOLD_KEYS.includes(e.key)) return;
        e.preventDefault();
        if (!e.repeat) start();
      },
      onKeyUp: (e) => {
        if (!enabled || !HOLD_KEYS.includes(e.key)) return;
        e.preventDefault();
        stop();
      },
    },
  };
}
