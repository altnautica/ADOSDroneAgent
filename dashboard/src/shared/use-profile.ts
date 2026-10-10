// The node profile, probed from `GET /api/pairing/info`. That route is public
// and served natively on every profile, so it answers even when the node
// refuses everything else, and it carries the resolved `profile`
// discriminator. The probe runs once per page load and retries on a fixed
// cadence until the agent answers (a kiosk can load before the API is up).
// `invalidateProfile()` drops the cache after a live profile change, and every
// mounted `useProfile()` probes again.

import { useEffect, useState } from "react";

import { apiFetch } from "./api-fetch";

export type AgentProfile = "drone" | "ground_station" | "workstation" | "compute" | "unknown";

export interface PairingInfoLite {
  profile?: string | null;
  pairing_code?: string | null;
  paired?: boolean | null;
  device_id?: string | null;
  name?: string | null;
}

export const PROFILE_RETRY_MS = 3000;

let cachedInfo: PairingInfoLite | null = null;
let inFlight: Promise<PairingInfoLite> | null = null;
let epoch = 0;
const listeners = new Set<() => void>();

/** Normalize the wire profile (`ground-station` on the wire) to the
 *  underscored discriminator the apps key on. */
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

/** The pairing info, fetched once and shared by every caller. Never resolves
 *  to a fabricated value: it waits and asks again until the agent answers. */
export function probePairingInfo(): Promise<PairingInfoLite> {
  if (cachedInfo) return Promise.resolve(cachedInfo);
  if (inFlight) return inFlight;
  const myEpoch = epoch;
  const probe = (async () => {
    for (;;) {
      try {
        const info =
          (await apiFetch<PairingInfoLite | null>("/api/pairing/info", { skipAuthSignal: true })) ??
          {};
        if (myEpoch === epoch) cachedInfo = info;
        return info;
      } catch {
        await new Promise((r) => setTimeout(r, PROFILE_RETRY_MS));
      }
    }
  })();
  inFlight = probe;
  void probe.finally(() => {
    if (inFlight === probe) inFlight = null;
  });
  return probe;
}

export async function probeProfile(): Promise<AgentProfile> {
  return normalizeProfile((await probePairingInfo()).profile);
}

/** Forget the probed profile (after a live profile change) and make every
 *  mounted `useProfile()` probe again. */
export function invalidateProfile(): void {
  epoch += 1;
  cachedInfo = null;
  inFlight = null;
  for (const l of listeners) l();
}

/** The resolved profile, or null until the agent first answers. */
export function useProfile(): AgentProfile | null {
  const [profile, setProfile] = useState<AgentProfile | null>(
    cachedInfo ? normalizeProfile(cachedInfo.profile) : null,
  );
  const [generation, setGeneration] = useState(0);

  useEffect(() => {
    const onInvalidate = () => {
      setProfile(null);
      setGeneration((g) => g + 1);
    };
    listeners.add(onInvalidate);
    return () => {
      listeners.delete(onInvalidate);
    };
  }, []);

  useEffect(() => {
    if (profile !== null) return;
    let cancelled = false;
    void probeProfile().then((p) => {
      if (!cancelled) setProfile(p);
    });
    return () => {
      cancelled = true;
    };
  }, [profile, generation]);

  return profile;
}
