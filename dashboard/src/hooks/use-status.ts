import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/shared/api-fetch";
import type { SetupStatus } from "@/lib/types";

/** Poll cadence on the pages that own the status (Home, Settings). */
export const STATUS_POLL_MS = 20_000;

/** `GET /api/v1/setup/status`. Only Home and the Settings layout pass
 *  `{ poll: true }`; every other reader fetches on mount and shares that
 *  cache, so the status endpoint is not polled from every page. */
export function useStatus({ poll = false }: { poll?: boolean } = {}) {
  return useQuery<SetupStatus>({
    queryKey: ["setup-status"],
    queryFn: ({ signal }) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal }),
    refetchInterval: poll ? STATUS_POLL_MS : false,
    staleTime: STATUS_POLL_MS,
  });
}
