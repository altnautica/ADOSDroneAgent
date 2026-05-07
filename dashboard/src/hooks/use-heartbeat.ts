import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/lib/api";
import type { AgentHeartbeat } from "@/lib/types";

export function useHeartbeat() {
  return useQuery<AgentHeartbeat>({
    queryKey: ["agent-heartbeat"],
    queryFn: ({ signal }) => apiFetch<AgentHeartbeat>("/api/status", { signal }),
    refetchInterval: 5_000,
    refetchOnWindowFocus: true,
  });
}
