// Pure helpers for the node identity (profile + ground role) and the
// profile gate decision. Kept free of React so they are unit-testable.

import type { GroundRole, Profile } from "@/shared/api/types";

export type AllowedProfile = "drone" | "ground_station" | "workstation" | "compute" | "visor";

/** The wire role (`/api/pairing/info.role`) in the dashboard vocabulary. A
 *  missing or unrecognised role reads `direct`, the ground-station default. */
export function groundRoleFrom(raw: string | null | undefined): GroundRole {
  if (raw === "relay" || raw === "receiver") return raw;
  return "direct";
}

export type GateDecision = "pass" | "mismatch" | "unavailable";

/** What kind of machine the agent runs on. `unknown` is a node whose status has
 *  not answered yet, and it passes: showing a page that turns out not to apply
 *  is recoverable, hiding the page an operator needs is not. */
export type HostKind = "appliance" | "personal" | "unknown";

/** `/api/status.host_kind` in the dashboard vocabulary. */
export function hostKindFrom(raw: unknown): HostKind {
  if (raw === "personal") return "personal";
  if (raw === "appliance") return "appliance";
  return "unknown";
}

/** Whether a page restricted to `allow` (and optionally to ground `roles` and
 *  to host `hosts`) renders on this node. Only `auto` (the agent has not
 *  answered yet) passes through ungated; `unknown` (a profile this dashboard
 *  does not know) is `unavailable`: no gated page applies to it, with no
 *  profile to switch to.
 *
 *  The host gate is the "does this page describe this machine" half: a ground
 *  station on somebody's own laptop runs no access point, HDMI kiosk or status
 *  panel, so those pages would configure services that are not there. An
 *  unanswered host passes, for the same reason `auto` does. */
export function gateDecision(
  profile: Profile,
  role: GroundRole,
  allow: readonly AllowedProfile[],
  roles?: readonly GroundRole[],
  hosts?: readonly HostKind[],
  host: HostKind = "unknown",
): GateDecision {
  if (profile === "auto") return "pass";
  if (profile === "unknown") return "unavailable";
  // An empty list is "no profile restriction", which is how a page gated only
  // on the host kind is expressed.
  if (allow.length > 0 && !allow.includes(profile)) return "mismatch";
  if (profile === "ground_station" && roles && roles.length > 0 && !roles.includes(role)) {
    return "mismatch";
  }
  if (hosts && hosts.length > 0 && host !== "unknown" && !hosts.includes(host)) {
    return "mismatch";
  }
  return "pass";
}
