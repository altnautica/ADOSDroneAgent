import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/lib/api";
import type { SetupStatus } from "@/lib/types";

export function useStatus() {
  return useQuery<SetupStatus>({
    queryKey: ["setup-status"],
    queryFn: ({ signal }) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal }),
    refetchInterval: 8_000,
  });
}
