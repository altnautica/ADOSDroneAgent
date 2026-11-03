// The Profile settings page: the profile and ground-station role draft, its
// dirty rule and the apply payload. Workstation and compute are detected,
// never chosen here.

import type { ApplyPayload, ApplyProfile } from "@/shared/api/apply-actions";
import type { GroundRole } from "@/shared/api/types";

export const PROFILE_OPTIONS = [
  { value: "drone", labelKey: "settings.profile.droneLabel", descriptionKey: "settings.profile.droneDescription" },
  {
    value: "ground_station",
    labelKey: "settings.profile.groundStationLabel",
    descriptionKey: "settings.profile.groundStationDescription",
  },
  { value: "visor", labelKey: "settings.profile.visorLabel", descriptionKey: "settings.profile.visorDescription" },
] as const satisfies readonly { value: ApplyProfile; labelKey: string; descriptionKey: string }[];

export const ROLE_OPTIONS = [
  { value: "direct", labelKey: "settings.profile.directLabel", descriptionKey: "settings.profile.directDescription" },
  { value: "relay", labelKey: "settings.profile.relayLabel", descriptionKey: "settings.profile.relayDescription" },
  {
    value: "receiver",
    labelKey: "settings.profile.receiverLabel",
    descriptionKey: "settings.profile.receiverDescription",
  },
] as const satisfies readonly { value: GroundRole; labelKey: string; descriptionKey: string }[];

export interface ProfileDraft {
  profile: ApplyProfile;
  role: GroundRole;
  autoRestart: boolean;
}

/** The role only counts on a ground station; auto-restart is an apply option,
 *  not a stored setting, so it never makes the page dirty. */
export function profileDirty(draft: ProfileDraft, initial: { profile: ApplyProfile; role: GroundRole }): boolean {
  return (
    draft.profile !== initial.profile || (draft.profile === "ground_station" && draft.role !== initial.role)
  );
}

/** The apply body; `force` is set only on the armed-override retry. */
export function profilePayload(draft: ProfileDraft, force: boolean): ApplyPayload {
  return {
    profile: {
      profile: draft.profile,
      ground_role: draft.profile === "ground_station" ? draft.role : undefined,
      auto_restart: draft.autoRestart,
    },
    force: force || undefined,
  };
}
