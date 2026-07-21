// The agent profile (drone / ground_station / …) probed once from the pairing
// info endpoint, which every profile serves and which carries the resolved
// `profile` discriminator. The cockpit is served on all profiles, so the shell
// reads this to shape itself: which status source to poll (a drone composes its
// own status; a ground station reads the composite) and which tabs to show
// (a drone hides the ground-station-only Mesh + Uplink screens).

import { useEffect, useState } from "react";

import { apiFetch } from "@/lib/api";

export type AgentProfile = "drone" | "ground_station" | "workstation" | "compute" | "unknown";

/** Module-level cache so the probe runs once per page load, not per component. */
let cached: AgentProfile | null = null;

interface PairingInfoLite {
  profile?: string | null;
}

/** Normalize the wire profile (which uses the hyphen form `ground-station`) to
 *  the underscored discriminator the rest of the app keys on. */
export function normalizeProfile(raw: string | null | undefined): AgentProfile {
  switch ((raw ?? "").trim()) {
    case "drone":
      return "drone";
    case "ground_station":
    case "ground-station":
      return "ground_station";
    case "workstation":
      return "workstation";
    case "compute":
      return "compute";
    default:
      return "unknown";
  }
}

/** The resolved agent profile, or `null` until the first probe returns. A failed
 *  probe leaves it `null`; callers treat `null` as "not yet known" and fall back
 *  to the ground-station shape (the historical default) until it resolves. */
export function useProfile(): AgentProfile | null {
  const [profile, setProfile] = useState<AgentProfile | null>(cached);

  useEffect(() => {
    if (cached !== null) return;
    let cancelled = false;
    apiFetch<PairingInfoLite>("/api/pairing/info")
      .then((info) => {
        const p = normalizeProfile(info.profile);
        cached = p;
        if (!cancelled) setProfile(p);
      })
      .catch(() => {
        // Leave null; the shell keeps the default shape until a later mount
        // retries. Never fabricate a profile.
      });
    return () => {
      cancelled = true;
    };
  }, []);

  return profile;
}
