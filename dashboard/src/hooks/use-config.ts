import { useQuery } from "@tanstack/react-query";

import type { AgentConfig } from "@/shared/api/types";
import { apiFetch } from "@/shared/api-fetch";

export function useConfig() {
  return useQuery<AgentConfig>({
    queryKey: ["config"],
    queryFn: () => apiFetch<AgentConfig>("/api/config"),
    staleTime: 30_000,
    refetchOnWindowFocus: false,
  });
}
