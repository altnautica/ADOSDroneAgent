// Controller rumble (C4) through the Gamepad API's `vibrationActuator`
// (`dual-rumble`): 1 × 80 ms on a mode or skill change, 2 short on a warning,
// 3 × 300 ms on a critical, single pulses for the hold ramp and the RTH
// countdown. `pilotFeedback` decides which pattern a pilot-state change earns,
// so the pad also reports what happens without a button press (an owner or
// skill change, a new warning).

import { RUMBLE, type RumblePulses } from "@/lib/pad/ds4-map";

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

/** The pilot-state fields feedback watches. */
export interface FeedbackView {
  owner: string | null;
  skillId: string | null;
  controlMode: string | null;
  armed: boolean;
  /** Ids of readiness items with severity `warn` that are not ok, plus the
   *  hold reason. */
  warnings: readonly string[];
  /** Ids of failing `block` items while armed, plus a running link-loss
   *  countdown. */
  criticals: readonly string[];
}

/** The pattern a change from `prev` to `next` earns (the strongest wins), or
 *  null. The first state seen earns nothing. */
export function pilotFeedback(prev: FeedbackView | null, next: FeedbackView): RumblePattern | null {
  if (!prev) return null;
  if (next.criticals.some((c) => !prev.criticals.includes(c))) return "critical";
  if (next.warnings.some((w) => !prev.warnings.includes(w))) return "warning";
  if (prev.owner !== next.owner || prev.skillId !== next.skillId || prev.controlMode !== next.controlMode) {
    return "mode_change";
  }
  return null;
}
