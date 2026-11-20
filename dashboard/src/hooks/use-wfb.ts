import { useQuery } from "@tanstack/react-query";

import { getWfbStatus } from "@/shared/api/wfb";

export const WFB_POLL_MS = 2_000;

/** `GET /api/wfb`, polled every 2 s while `enabled` (default on). */
export function useWfb({ enabled = true }: { enabled?: boolean } = {}) {
  return useQuery({
    queryKey: ["wfb", "status"],
    queryFn: () => getWfbStatus(),
    refetchInterval: WFB_POLL_MS,
    enabled,
  });
}
