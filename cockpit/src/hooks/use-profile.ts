// The agent profile (drone / ground_station / …) probed once from the pairing
// info endpoint, which every profile serves and which carries the resolved
// `profile` discriminator. The cockpit is served on all profiles, so the shell
// reads this to shape itself: which status source to poll (a drone composes its
// own status; a ground station reads the composite) and which tabs to show
// (a drone hides the ground-station-only Mesh + Uplink screens).

import { useEffect, useState } from "react";

import { apiFetch } from "@/lib/api";
import { useReachStore } from "@/stores/reach-store";

export type AgentProfile = "drone" | "ground_station" | "workstation" | "compute" | "unknown";

/** Module-level cache so the probe runs once per page load, not per component. */
let cached: AgentProfile | null = null;

/** Fixed pause between probes while the agent has not answered yet (the kiosk
 *  can load before the API is up). */
export const PROFILE_RETRY_MS = 3000;

/** One shared probe: every hook instance waits on the same request, and it is
 *  retried on a fixed cadence until the agent answers. */
let inFlight: Promise<AgentProfile> | null = null;

export function probeProfile(): Promise<AgentProfile> {
  if (cached !== null) return Promise.resolve(cached);
  if (inFlight) return inFlight;
  inFlight = (async () => {
    for (;;) {
      try {
        const info = await apiFetch<PairingInfoLite>("/api/pairing/info");
        const p = normalizeProfile(info.profile);
        cached = p;
        // Stash the code alongside the profile. A node that refuses every other
        // call still answers this one, so this is where the operator's way out
        // comes from.
        useReachStore.getState().setPairingCode(info.pairing_code ?? null);
        return p;
      } catch {
        // Not answering yet (booting agent, link down). Never fabricate a
        // profile; wait and ask again.
        await new Promise((r) => setTimeout(r, PROFILE_RETRY_MS));
      }
    }
  })();
  return inFlight;
}

interface PairingInfoLite {
  profile?: string | null;
  /** Present on every profile; carried here because this probe is the ONE call
   *  that still succeeds when the node is refusing everything else, so it is
   *  the only chance to learn the code that would end the refusal. */
  pairing_code?: string | null;
  paired?: boolean | null;
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

/** The resolved agent profile, or `null` until the agent first answers. The
 *  probe retries on a fixed cadence until it does, so a panel that loaded
 *  before the API was up still learns its profile; callers treat `null` as
 *  "not yet known". */
export function useProfile(): AgentProfile | null {
  const [profile, setProfile] = useState<AgentProfile | null>(cached);

  useEffect(() => {
    if (cached !== null) return;
    let cancelled = false;
    void probeProfile().then((p) => {
      if (!cancelled) setProfile(p);
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return profile;
}
