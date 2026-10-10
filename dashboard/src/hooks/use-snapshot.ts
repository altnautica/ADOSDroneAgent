import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/shared/api-fetch";
import type { DashboardSnapshot } from "@/lib/types";

export interface SnapshotOptions {
  /** Poll cadence; the live panels default to 1 Hz. */
  intervalMs?: number;
  /** False keeps the observer idle (no fetch, no poll). */
  enabled?: boolean;
}

export function useSnapshot({ intervalMs = 1_000, enabled = true }: SnapshotOptions = {}) {
  return useQuery<DashboardSnapshot>({
    queryKey: ["dashboard-snapshot"],
    queryFn: ({ signal }) =>
      apiFetch<DashboardSnapshot>("/api/v1/dashboard/snapshot", { signal }),
    refetchInterval: intervalMs,
    enabled,
  });
}
