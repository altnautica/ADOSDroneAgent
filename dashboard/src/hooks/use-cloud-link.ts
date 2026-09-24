import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/lib/api";

/** `GET /api/cloud/link`: the cloud relay's broker session and last status
 * POST. The route answers 404 while the relay is not reporting. */
export interface CloudLink {
  paired: boolean;
  cloud_url_set: boolean;
  broker_connected: boolean | null;
  last_heartbeat_ok_ms: number | null;
  last_heartbeat_status: number | null;
  last_heartbeat_error: string | null;
}

export function useCloudLink(enabled: boolean) {
  return useQuery<CloudLink>({
    queryKey: ["cloud-link"],
    queryFn: ({ signal }) => apiFetch<CloudLink>("/api/cloud/link", { signal }),
    refetchInterval: 5_000,
    retry: false,
    enabled,
  });
}
