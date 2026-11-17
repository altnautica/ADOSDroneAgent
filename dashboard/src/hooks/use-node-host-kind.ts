/**
 * What kind of machine the agent runs on, from its own `/api/status`.
 *
 * A ground station on somebody's own laptop or desktop is a different node from
 * one on a dedicated box: it runs no access point, no HDMI kiosk and no status
 * panel, because each of those would take a machine its owner is using. The
 * dashboard uses this to keep those pages off a node they do not describe.
 *
 * `unknown` until the status answers (and on any failure), which the gates treat
 * as "do not hide anything": a page that turns out not to apply is recoverable,
 * while hiding the page an operator needs is not.
 */

import { useQuery } from "@tanstack/react-query";

import { apiFetch } from "@/shared/api-fetch";
import { hostKindFrom, type HostKind } from "@/lib/node-identity";

interface StatusHostFields {
  host_kind?: unknown;
}

/** The node's host kind. Cached briefly: it changes only on a reinstall. */
export function useNodeHostKind(): HostKind {
  const { data } = useQuery<StatusHostFields>({
    queryKey: ["status-host"],
    queryFn: ({ signal }) => apiFetch<StatusHostFields>("/api/status", { signal }),
    staleTime: 60_000,
  });
  return hostKindFrom(data?.host_kind);
}
