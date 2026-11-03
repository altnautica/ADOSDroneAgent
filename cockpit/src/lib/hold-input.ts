// Completing a hold from the panel's own buttons. A guarded button or a slide
// to confirm that holds focus registers itself here; a long press of the
// panel's select button then completes it in one gesture, the same way it
// completes the skill confirm sheet. Pointer, keyboard and gamepad holds are
// timed by the control itself.

let target: (() => void) | null = null;

/** Make `complete` the hold the next panel long-press finishes. Returns the
 *  release, which only clears the slot when it still holds this callback. */
export function claimPanelHold(complete: () => void): () => void {
  target = complete;
  return () => {
    if (target === complete) target = null;
  };
}

/** Finish the claimed hold. True when a control took it. */
export function completePanelHold(): boolean {
  if (!target) return false;
  const complete = target;
  target = null;
  complete();
  return true;
}
