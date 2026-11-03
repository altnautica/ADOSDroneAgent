// The pure half of the PIN access gate: when the form may submit, how a submit
// result maps onto what the operator sees, and the lockout countdown. The PIN
// routes and their wire shapes live in `@/shared/api/pin`.

import type { LockScope, PinSubmitResult, SplashMode } from "@/shared/api/pin";
import { isValidPin } from "@/shared/api/pin";

export interface PinForm {
  mode: SplashMode;
  pin: string;
  confirm: string;
  busy: boolean;
  locked: boolean;
}

/** Whether the form can submit: a valid PIN, and in `set` mode a valid second
 *  entry too. A locked, unknown or busy form never submits. */
export function canSubmitPin(f: PinForm): boolean {
  if (f.busy || f.locked) return false;
  if (f.mode === "set") return isValidPin(f.pin) && isValidPin(f.confirm);
  if (f.mode === "enter") return isValidPin(f.pin);
  return false;
}

/** In `set` mode the two entries must match before anything is sent. */
export function pinMismatch(f: PinForm): boolean {
  return f.mode === "set" && f.pin !== f.confirm;
}

export type PinFeedback =
  | { kind: "unlocked" }
  | { kind: "locked"; lockedUntil: number; scope: LockScope }
  | { kind: "wrong"; remaining: number }
  | { kind: "message"; message: string };

export function pinFeedback(res: PinSubmitResult): PinFeedback {
  if (res.ok) return { kind: "unlocked" };
  switch (res.kind) {
    case "locked":
      return { kind: "locked", lockedUntil: res.lockedUntil, scope: res.scope };
    case "wrong":
      return { kind: "wrong", remaining: res.remaining };
    default:
      return { kind: "message", message: res.message };
  }
}

/** Whole seconds left in a lockout ending at `until` (unix seconds). */
export function lockRemainingS(until: number | null, nowMs: number): number {
  if (until == null) return 0;
  return Math.max(0, Math.ceil(until - nowMs / 1000));
}

export function fmtMMSS(totalS: number): string {
  const s = Math.max(0, Math.floor(totalS));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}
