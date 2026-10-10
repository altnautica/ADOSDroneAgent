import { queryOptions, useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/shared/api-fetch";

/** The react-query options behind `useResource`. The cache key carries the
 *  path as well as the name, so two readers that reuse a name for different
 *  endpoints never share (and overwrite) one cache entry. */
export function resourceQueryOptions<T>(key: string, path: string, refetchMs: number | false = false) {
  return queryOptions<T>({
    queryKey: [key, path],
    queryFn: ({ signal }) => apiFetch<T>(path, { signal }),
    refetchInterval: refetchMs || false,
    staleTime: 5_000,
  });
}

// Tiny shared hook for read-only resource endpoints. Each panel below
// uses this to fetch its slice. Polling is opt-in via refetchMs since
// most of these views can rely on staleTime + manual refresh.
export function useResource<T>(key: string, path: string, refetchMs: number | false = false) {
  return useQuery(resourceQueryOptions<T>(key, path, refetchMs));
}
