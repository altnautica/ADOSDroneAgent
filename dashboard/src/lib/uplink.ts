// REST wrappers + response types for the ground-station uplink matrix: the
// aggregate network view, the failover priority list, and the uplink-sharing
// flag. These routes are profile-gated on the agent (they 404 on a drone /
// workstation), so the Network panel only calls them once it knows the node is
// a ground station.

import { apiFetch } from "./api";

// /api/v1/ground-station/network — the aggregate uplink matrix. Each leg is the
// agent's own report; `active_uplink` is the daemon's authoritative selection.
export interface GsApLeg {
  enabled?: boolean;
  running?: boolean;
  ssid?: string | null;
  channel?: number | null;
  interface?: string | null;
  gateway?: string | null;
  standing_down?: boolean;
  standdown_reason?: string | null;
}

export interface GsWifiClientLeg {
  enabled_on_boot?: boolean;
  connected?: boolean;
  ssid?: string | null;
  signal?: number | null;
  ip?: string | null;
}

export interface GsModemLeg {
  enabled?: boolean;
  connected?: boolean;
  iface?: string | null;
  ip?: string | null;
  signal_quality?: number | null;
  technology?: string | null;
  apn?: string | null;
  operator?: string | null;
  data_used_mb?: number;
  cap_mb?: number;
  percent?: number;
  state?: string | null;
}

export interface GsNetworkView {
  ap?: GsApLeg;
  wifi_client?: GsWifiClientLeg;
  modem_4g?: GsModemLeg;
  // The agent's own selected uplink token (e.g. "eth0", "wlan0_client"), or
  // null when it has not reported one. Never derived client-side.
  active_uplink?: string | null;
  priority?: string[];
  share_uplink?: boolean;
}

// PUT /api/v1/ground-station/network/priority — the persisted ordered list.
export interface UplinkPriorityResult {
  priority: string[];
}

// PUT /api/v1/ground-station/network/share_uplink — persisted flag + apply state.
export interface ShareUplinkResult {
  enabled: boolean;
  applied?: boolean;
  apply_error?: string | null;
  backend?: string;
}

export function getGsNetwork() {
  return apiFetch<GsNetworkView>("/api/v1/ground-station/network");
}

// Persist the ordered uplink priority list. The response is the persisted list
// (the read-back), so callers render the returned order, not the optimistic one.
export function setUplinkPriority(priority: string[]) {
  return apiFetch<UplinkPriorityResult>(
    "/api/v1/ground-station/network/priority",
    { method: "PUT", body: { priority } },
  );
}

// Toggle sharing the active uplink with AP clients (NAT). The response carries
// the persisted flag plus whether the daemon actually applied it.
export function setShareUplink(enabled: boolean) {
  return apiFetch<ShareUplinkResult>(
    "/api/v1/ground-station/network/share_uplink",
    { method: "PUT", body: { enabled } },
  );
}
