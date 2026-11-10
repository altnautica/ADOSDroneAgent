// The emergency stop's confirm sheet texts, the same on every surface:
// title `cockpit.estop`, body `confirm.kill.message`, one hold button
// labelled `stop.hold_label` at rest that counts down (`hold.keep_holding`)
// while held.

import { specText } from "@/shared/hud/spec-text";

export function stopSheetTitle(): string {
  return specText("cockpit.estop");
}

export function stopSheetBody(): string {
  return specText("confirm.kill.message");
}

/** The hold button's text: the spec label at rest, the time still to hold
 *  (one decimal) while held. */
export function stopHoldText(holding: boolean, holdMs: number, progress: number): string {
  if (!holding) return specText("stop.hold_label");
  const left = holdMs * (1 - Math.min(1, Math.max(0, progress)));
  return specText("hold.keep_holding", { s: (left / 1000).toFixed(1) });
}
