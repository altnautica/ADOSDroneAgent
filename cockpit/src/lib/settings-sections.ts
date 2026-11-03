// The agent settings sections reachable from Settings as `settings:!<id>`.
// Each is a purpose-built page (ported from the node dashboard) for one slice
// of the agent's configuration; the raw "All settings" tree stays beside them.
// This module is the catalogue and its visibility rule; the page components
// are bound lazily in `components/settings/sections/registry.ts`.

import type { GroundRole } from "@/shared/api/types";
import type { AgentProfile } from "@/shared/use-profile";

export const SETTINGS_SECTION_IDS = [
  "profile",
  "region",
  "battery",
  "wifi",
  "hotspot",
  "uplink",
  "cellular",
  "macPin",
  "cloud",
  "selfHeal",
  "mavlink",
  "security",
  "vision",
  "camera",
  "capture",
  "sensors",
  "swarm",
  "discovery",
  "display",
  "advanced",
] as const;

export type SettingsSectionId = (typeof SETTINGS_SECTION_IDS)[number];

type KnownProfile = Exclude<AgentProfile, "unknown">;

export interface SettingsSectionSpec {
  id: SettingsSectionId;
  /** Profiles the section applies to; absent = every profile. */
  profiles?: readonly KnownProfile[];
  /** Ground-station roles the section applies to; absent = every role. */
  roles?: readonly GroundRole[];
}

const AIR: readonly KnownProfile[] = ["drone", "visor"];

export const SETTINGS_SECTIONS: readonly SettingsSectionSpec[] = [
  { id: "profile" },
  { id: "region", profiles: ["drone", "ground_station"] },
  { id: "battery", profiles: AIR },
  { id: "wifi" },
  { id: "hotspot" },
  { id: "uplink", profiles: ["ground_station"] },
  { id: "cellular" },
  { id: "macPin" },
  { id: "cloud" },
  { id: "selfHeal" },
  { id: "mavlink", profiles: AIR },
  { id: "security" },
  { id: "vision", profiles: AIR },
  { id: "camera", profiles: AIR },
  { id: "capture", profiles: ["visor"] },
  { id: "sensors", profiles: AIR },
  { id: "swarm", profiles: ["drone"] },
  { id: "discovery" },
  { id: "display", profiles: ["ground_station"] },
  { id: "advanced" },
];

export function isSettingsSectionId(id: string): id is SettingsSectionId {
  return (SETTINGS_SECTION_IDS as readonly string[]).includes(id);
}

/** Whether a section shows for this node. An unknown profile (still probing,
 *  or unrecognised) shows only the profile-agnostic sections; a ground-station
 *  role that is not yet known passes a role gate rather than flicker it away. */
export function sectionVisibleFor(
  spec: SettingsSectionSpec,
  profile: AgentProfile | null,
  role: GroundRole | null,
): boolean {
  if (spec.profiles) {
    if (profile == null || profile === "unknown") return false;
    if (!spec.profiles.includes(profile)) return false;
  }
  if (spec.roles && profile === "ground_station" && role != null) {
    return spec.roles.includes(role);
  }
  return true;
}

export function visibleSections(
  profile: AgentProfile | null,
  role: GroundRole | null,
): SettingsSectionSpec[] {
  return SETTINGS_SECTIONS.filter((s) => sectionVisibleFor(s, profile, role));
}
