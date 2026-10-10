import { useQuery } from "@tanstack/react-query";

import { useNodeProfile } from "@/hooks/use-node-profile";
import type { PairingInfo } from "@/hooks/use-pairing";
import { groundRoleFrom } from "@/lib/node-identity";
import type { GroundRole, Profile } from "@/lib/types";
import { apiFetch } from "@/shared/api-fetch";

export interface NodeIdentity {
  /** Resolved profile: `auto` until the agent first answers, `unknown` for a
   *  profile with no dashboard pages (workstation, compute). */
  profile: Profile;
  /** Ground-station role; `direct` when the node reports none. */
  role: GroundRole;
  name: string | null;
  mdnsHost: string | null;
}

/** The node's resolved identity, read from the public `/api/pairing/info`
 *  probe (the same source the sidebar and ProfileGate use). It never reads
 *  the raw `agent.profile` config value from the setup status. Shares the
 *  `pairing-info` cache with `usePairingInfo()` and does not poll on its own. */
export function useNodeIdentity(): NodeIdentity {
  const profile = useNodeProfile();
  const info = useQuery<PairingInfo>({
    queryKey: ["pairing-info"],
    queryFn: ({ signal }) => apiFetch<PairingInfo>("/api/pairing/info", { signal }),
    staleTime: 30_000,
  });
  return {
    profile,
    role: groundRoleFrom(info.data?.role),
    name: info.data?.name?.trim() || null,
    mdnsHost: info.data?.mdns_host?.trim() || null,
  };
}
