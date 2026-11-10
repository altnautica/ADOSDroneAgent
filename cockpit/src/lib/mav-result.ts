// A flight controller's command acknowledgement in words: the FC's own status
// text when it sent one, else the spec's word for the MAV_RESULT name
// (`mav_result.*`). A raw result id is never shown.

import { hasSpecText, specText } from "@/shared/hud/spec-text";

export function ackText(ack: { statustext?: string | null; result_name?: string | null } | null | undefined): string | undefined {
  const status = ack?.statustext?.trim();
  if (status) return status;
  const name = ack?.result_name?.replace(/^MAV_RESULT_/, "");
  if (!name) return undefined;
  const key = `mav_result.${name}`;
  return hasSpecText(key) ? specText(key) : specText("refusal.refused");
}
