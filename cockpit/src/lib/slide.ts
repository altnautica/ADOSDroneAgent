// Slide-to-confirm completion: the drag has to start on the thumb (the left
// end of the track) and travel almost the whole track. A tap anywhere,
// including the far end, never completes it.

export const SLIDE_START_MAX = 0.15;
export const SLIDE_COMPLETE = 0.92;

export function slideCompletes(startFrac: number | null, frac: number): boolean {
  return startFrac !== null && startFrac < SLIDE_START_MAX && frac >= SLIDE_COMPLETE;
}
