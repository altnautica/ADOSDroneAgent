import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/shared/api-fetch";
import type { AgentHeartbeat } from "@/shared/api/types";

export const HEARTBEAT_POLL_MS = 5_000;

export function useHeartbeat() {
  return useQuery<AgentHeartbeat>({
    queryKey: ["agent-heartbeat"],
    queryFn: ({ signal }) => apiFetch<AgentHeartbeat>("/api/status", { signal }),
    refetchInterval: HEARTBEAT_POLL_MS,
  });
}
