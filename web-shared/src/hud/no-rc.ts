// The no-RC posture: while the drone is declared to fly with no RC
// transmitter (`rc_receiver: absent`), the "No RC" chip and the line saying
// what stops the drone and what a lost link ends in. The link-loss action and
// window come from the pilot state, or the pilot's defaults (return home after
// 10 s) from an agent that omits them.

import type { PilotState } from "./cockpit-frames";
import { linkLossActionText } from "./link-loss";
import { specText } from "./spec-text";

/** The link-loss window the pilot uses when its state does not report one. */
export const DEFAULT_LINK_LOSS_S = 10;

type NoRcInputs = Pick<PilotState, "rcReceiver" | "linkLossAction" | "linkLossS">;

/** "No RC" while the drone flies with no RC transmitter, else null. */
export function noRcChipText(p: Pick<PilotState, "rcReceiver">): string | null {
  return p.rcReceiver === "absent" ? specText("chip.no_rc") : null;
}

/** The posture line with no RC transmitter, else null. */
export function noRcStripText(p: NoRcInputs): string | null {
  if (p.rcReceiver !== "absent") return null;
  return specText("follow.no_rc_strip", {
    action: linkLossActionText(p.linkLossAction ?? "rth"),
    s: Math.round(p.linkLossS ?? DEFAULT_LINK_LOSS_S),
  });
}
