// The redetect answer: the profile (and ground role) the hardware points at,
// offered as a suggestion the operator applies through the usual confirm.

import { profileLabel, roleLabel } from "@/lib/enum-labels";
import type { ApplyProfile } from "@/shared/api/apply-actions";
import type { GroundRole } from "@/shared/api/types";

export interface ProfileSuggestion {
  detected: ApplyProfile;
  ground_role_hint?: GroundRole;
}

/** The suggestion in words: "Ground station · Relay", "Drone". */
export function suggestionLabel(s: ProfileSuggestion): string {
  return s.detected === "ground_station" && s.ground_role_hint
    ? `${profileLabel(s.detected)} · ${roleLabel(s.ground_role_hint)}`
    : profileLabel(s.detected);
}

/** The draft "Apply suggestion" loads before the confirm opens. A suggestion
 *  without a role hint keeps the role the operator has drafted. */
export function suggestionDraft(
  s: ProfileSuggestion,
  draftRole: GroundRole,
): { profile: ApplyProfile; role: GroundRole } {
  return { profile: s.detected, role: s.ground_role_hint ?? draftRole };
}
