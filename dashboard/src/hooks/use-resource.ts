import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/lib/api";

// Tiny shared hook for read-only resource endpoints. Each panel below
// uses this to fetch its slice. Polling is opt-in via refetchMs since
// most of these views can rely on staleTime + manual refresh.
export function useResource<T>(
  key: string,
  path: string,
  refetchMs: number | false = false,
) {
  return useQuery<T>({
    queryKey: [key],
    queryFn: ({ signal }) => apiFetch<T>(path, { signal }),
    refetchInterval: refetchMs || false,
    staleTime: 5_000,
  });
}
