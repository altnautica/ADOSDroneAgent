import { useQuery } from "@tanstack/react-query";

import { getWfbStatus } from "@/lib/wfb";

export function useWfb() {
  return useQuery({
    queryKey: ["wfb", "status"],
    queryFn: () => getWfbStatus(),
    refetchInterval: 2_000,
  });
}
