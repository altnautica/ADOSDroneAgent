// Pure helpers for the node identity (profile + ground role) and the
// profile gate decision. Kept free of React so they are unit-testable.

import type { GroundRole, Profile } from "./types";

export type AllowedProfile = "drone" | "ground_station";

/** The wire role (`/api/pairing/info.role`) in the dashboard vocabulary. A
 *  missing or unrecognised role reads `direct`, the ground-station default. */
export function groundRoleFrom(raw: string | null | undefined): GroundRole {
  if (raw === "relay" || raw === "receiver") return raw;
  return "direct";
}

export type GateDecision = "pass" | "mismatch";

/** Whether a page restricted to `allow` (and optionally to ground `roles`)
 *  renders on this node. Only `auto` (the agent has not answered yet) passes
 *  through ungated; `unknown` is a known non-flight profile (workstation,
 *  compute) and is a mismatch, since those nodes do not serve the page. */
export function gateDecision(
  profile: Profile,
  role: GroundRole,
  allow: readonly AllowedProfile[],
  roles?: readonly GroundRole[],
): GateDecision {
  if (profile === "auto") return "pass";
  if (profile === "unknown") return "mismatch";
  if (!allow.includes(profile)) return "mismatch";
  if (profile === "ground_station" && roles && roles.length > 0 && !roles.includes(role)) {
    return "mismatch";
  }
  return "pass";
}
