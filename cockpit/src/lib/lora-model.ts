// The LoRa screen's region write. A region change switches the plan the lane
// transmits under, so it goes out through a slide, held while the vehicle is
// armed (the agent refuses it then too, `E_ARMED`). The picker's empty choice
// means "follow the country setting", sent as `null`.

import type { RadioWriteBlock } from "./radio-gate";

/** The picker value that follows the country setting. */
export const FOLLOW_COUNTRY = "";

/** The region body a picker choice sends. */
export function regionBody(choice: string): string | null {
  return choice === FOLLOW_COUNTRY ? null : choice;
}

export type RegionApplyBlock = Exclude<RadioWriteBlock, null> | "unchanged" | null;

/** Why the region slide is unavailable, or null when `choice` may be sent. */
export function regionApplyBlock(choice: string, current: string, gate: RadioWriteBlock): RegionApplyBlock {
  if (choice === current) return "unchanged";
  return gate;
}
