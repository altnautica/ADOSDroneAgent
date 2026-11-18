// The cockpit's one redraw beat. Everything on screen that changes on its
// own (clocks, freshness ages, the HUD repaint check, the status-pill flash)
// changes on this beat, so the chrome costs at most one frame per beat when
// nothing new arrives. A single shared interval runs while anything listens;
// every listener is called in the same task, so React batches their updates
// into one render and one frame.

import { useSyncExternalStore } from "react";

/** The beat period. */
export const BEAT_MS = 100;

let beat = 0;
let timer: ReturnType<typeof setInterval> | null = null;
const listeners = new Set<() => void>();

function tick(): void {
  beat += 1;
  for (const listener of listeners) listener();
}

/** Call `listener` on every beat until the returned function is called. The
 *  interval starts with the first listener and stops after the last. */
export function subscribeBeat(listener: () => void): () => void {
  listeners.add(listener);
  if (timer === null) timer = setInterval(tick, BEAT_MS);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && timer !== null) {
      clearInterval(timer);
      timer = null;
    }
  };
}

/** The beat count so far. */
export function beatNow(): number {
  return beat;
}

/** The beat count divided by `periodBeats`: a component re-renders only when
 *  this phase changes, every `periodBeats` beats. */
export function beatPhase(periodBeats: number): number {
  return Math.floor(beat / periodBeats);
}

export function useBeatPhase(periodBeats: number): number {
  return useSyncExternalStore(subscribeBeat, () => beatPhase(periodBeats));
}
