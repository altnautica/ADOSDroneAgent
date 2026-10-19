// Pure helpers for the node identity (profile + ground role) and the
// profile gate decision. Kept free of React so they are unit-testable.

import type { GroundRole, Profile } from "./types";

export type AllowedProfile = "drone" | "ground_station" | "workstation" | "compute" | "visor";

/** The wire role (`/api/pairing/info.role`) in the dashboard vocabulary. A
 *  missing or unrecognised role reads `direct`, the ground-station default. */
export function groundRoleFrom(raw: string | null | undefined): GroundRole {
  if (raw === "relay" || raw === "receiver") return raw;
  return "direct";
}

export type GateDecision = "pass" | "mismatch" | "unavailable";

/** Whether a page restricted to `allow` (and optionally to ground `roles`)
 *  renders on this node. Only `auto` (the agent has not answered yet) passes
 *  through ungated; `unknown` (a profile this dashboard does not know) is
 *  `unavailable`: no gated page applies to it, with no profile to switch to. */
export function gateDecision(
  profile: Profile,
  role: GroundRole,
  allow: readonly AllowedProfile[],
  roles?: readonly GroundRole[],
): GateDecision {
  if (profile === "auto") return "pass";
  if (profile === "unknown") return "unavailable";
  if (!allow.includes(profile)) return "mismatch";
  if (profile === "ground_station" && roles && roles.length > 0 && !roles.includes(role)) {
    return "mismatch";
  }
  return "pass";
}
