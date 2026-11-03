// The Discovery settings page model. Turning mDNS off withdraws the name the
// operator may be reaching this node by, so disabling it is held behind a
// guarded button; enabling writes at once. The reach facts come from
// `GET /api/v1/setup/status` and are shown only as the agent advertises them.

import type { SetupAccessUrl, SetupStatus } from "@/shared/agent-api.generated";

export const MDNS_KEY = "discovery.mdns_enabled";

/** How often the page re-reads the setup status. */
export const SETUP_STATUS_POLL_MS = 20_000;

/** True when writing `next` to the mDNS switch must wait for the guard. */
export function mdnsNeedsGuard(next: boolean): boolean {
  return next === false;
}

export interface ReachFacts {
  /** The published mDNS name, "" when none is published. */
  mdnsHost: string;
  /** The hostname, when it differs from the mDNS name. */
  hostname: string | null;
  /** The LAN host, when it differs from the mDNS name. */
  lanHost: string | null;
  localIps: string[];
  urls: SetupAccessUrl[];
}

export function reachFacts(status: SetupStatus | null): ReachFacts {
  const net = status?.network;
  const mdnsHost = net?.mdns_host?.trim() ?? "";
  const hostname = net?.hostname?.trim() ?? "";
  const lanHost = status?.lan_host?.trim() ?? "";
  return {
    mdnsHost,
    hostname: hostname && hostname !== mdnsHost ? hostname : null,
    lanHost: lanHost && lanHost !== mdnsHost ? lanHost : null,
    localIps: net?.local_ips ?? [],
    urls: status?.access_urls ?? [],
  };
}

/** A stable list key for an advertised URL. */
export function reachKey(entry: SetupAccessUrl, idx: number): string {
  return `${entry.kind}-${entry.id || entry.url}-${idx}`;
}
