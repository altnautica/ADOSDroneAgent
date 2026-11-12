import { useQuery } from "@tanstack/react-query";

import type { CatalogResponse } from "@/lib/plugin-catalog";
import { apiFetch } from "@/shared/api-fetch";

/** The bundled extension catalog. It is compiled into the agent, so it cannot
 *  change while the page is open: fetch once, never poll. */
export function usePluginCatalog() {
  return useQuery<CatalogResponse>({
    queryKey: ["plugin-catalog"],
    queryFn: ({ signal }) => apiFetch<CatalogResponse>("/api/v1/plugins/catalog", { signal }),
    staleTime: Infinity,
  });
}
