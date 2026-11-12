import { useQuery } from "@tanstack/react-query";

import { getWfbStatus } from "@/shared/api/wfb";

export const WFB_POLL_MS = 2_000;

export function useWfb() {
  return useQuery({
    queryKey: ["wfb", "status"],
    queryFn: () => getWfbStatus(),
    refetchInterval: WFB_POLL_MS,
  });
}
