// The gamepad's button states, published once per animation frame by the one
// gamepad poller (`useGamepad`) so the Feed's skill bindings and the bindings
// editor read the same sample without a second `getGamepads()` loop.

type Listener = (pressed: readonly boolean[], prev: readonly boolean[]) => void;

const listeners = new Set<Listener>();
let last: readonly boolean[] = [];

export function publishGamepadButtons(pressed: readonly boolean[]): void {
  const prev = last;
  last = pressed;
  for (const l of listeners) l(pressed, prev);
}

/** Subscribe to per-frame button samples. Returns an unsubscribe. */
export function onGamepadButtons(listener: Listener): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** Indices that went from released to pressed between two samples. */
export function risingEdges(pressed: readonly boolean[], prev: readonly boolean[]): number[] {
  const out: number[] = [];
  pressed.forEach((p, i) => {
    if (p && !prev[i]) out.push(i);
  });
  return out;
}
