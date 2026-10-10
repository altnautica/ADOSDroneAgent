import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/shared/api-fetch";
import type { DashboardSnapshot } from "@/lib/types";

export function useSnapshot() {
  return useQuery<DashboardSnapshot>({
    queryKey: ["dashboard-snapshot"],
    queryFn: ({ signal }) =>
      apiFetch<DashboardSnapshot>("/api/v1/dashboard/snapshot", { signal }),
    refetchInterval: 1_000,
  });
}
