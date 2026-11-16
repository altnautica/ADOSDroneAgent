// Controller rumble through the Gamepad API's `vibrationActuator`
// (`dual-rumble`): 1 × 80 ms on a mode change, 2 short on a warning,
// 3 × 300 ms on a critical, single pulses for the hold ramp and the RTH
// countdown. Which pattern a pilot-state change earns is the shared feedback
// table (`@/shared/hud/pilot-feedback`), driven by `PilotFeedback`.

import { RUMBLE, type RumblePulses } from "@/lib/pad/ds4-map";
import { feedbackAlert } from "@/shared/hud/pilot-feedback";
import { pilotFresh } from "@/shared/hud/pilot-fresh";

/** Runs the feedback table over the pilot frames a surface receives: each
 *  frame against the previous one, and a pilot state gone stale as a lost
 *  state (critical, once; the first frame after it earns nothing). */
export class PilotFeedback {
  private last: unknown = null;

  constructor(private readonly play: (pattern: RumblePattern) => void) {}

  private feed(next: unknown): void {
    const pattern = feedbackAlert(this.last, next);
    this.last = next;
    if (pattern) this.play(pattern);
  }

  /** A pilot frame received at `atMs` (wall clock), read at `nowMs`. A
   *  frame already stale (the HTTP fallback hands back the last one every
   *  poll) is no news and never re-arms the lost-state rumble. */
  frame(raw: unknown, atMs: number, nowMs: number): void {
    if (pilotFresh(atMs, nowMs)) this.feed(raw);
  }

  /** Each poll: the newest frame's receipt time, or undefined with none. */
  tick(atMs: number | undefined, nowMs: number): void {
    if (this.last !== null && !pilotFresh(atMs, nowMs)) this.feed(null);
  }
}

export type RumblePattern = "mode_change" | "warning" | "critical";

interface DualRumbleActuator {
  playEffect: (
    type: "dual-rumble",
    params: { duration: number; startDelay?: number; strongMagnitude?: number; weakMagnitude?: number },
  ) => Promise<unknown>;
}

function actuatorOf(pad: Gamepad | null): DualRumbleActuator | null {
  const a = (pad as unknown as { vibrationActuator?: Partial<DualRumbleActuator> } | null)?.vibrationActuator;
  return a && typeof a.playEffect === "function" ? (a as DualRumbleActuator) : null;
}

/** Whether this pad can rumble at all. */
export function canRumble(pad: Gamepad | null): boolean {
  return actuatorOf(pad) !== null;
}

/** The pulse schedule of a pattern: `[startDelay, duration]` pairs. */
export function pulseSchedule(p: RumblePulses): [number, number][] {
  let at = 0;
  return p.pulses_ms.map((ms) => {
    const slot: [number, number] = [at, ms];
    at += ms + p.gap_ms;
    return slot;
  });
}

function play(pad: Gamepad | null, startDelay: number, duration: number, strong: number, weak: number): void {
  const a = actuatorOf(pad);
  if (!a) return;
  a.playEffect("dual-rumble", { startDelay, duration, strongMagnitude: strong, weakMagnitude: weak }).catch(() => {
    // A pad that drops mid-effect rejects; nothing to do.
  });
}

export function playPattern(pad: Gamepad | null, pattern: RumblePattern): void {
  const p = RUMBLE[pattern];
  for (const [delay, ms] of pulseSchedule(p)) play(pad, delay, ms, p.strong, p.weak);
}

export function playPulse(pad: Gamepad | null, intensity: number, ms: number): void {
  const v = Math.max(0, Math.min(1, intensity));
  play(pad, 0, ms, v, v);
}
