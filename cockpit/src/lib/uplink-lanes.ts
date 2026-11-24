import type { Tone } from "@/lib/tone";

// The ground-station uplink lanes, keyed by the tokens the agent's uplink
// router actually reports: `active_uplink` and every `priority` entry are
// interface-role names (eth0 / wlan0_client / wwan0 / usb0), not lane words.

export type UplinkLane = "ethernet" | "wifi" | "modem" | "usb";

const LANE_BY_TOKEN: Record<string, UplinkLane> = {
  eth0: "ethernet",
  wlan0_client: "wifi",
  wwan0: "modem",
  usb0: "usb",
  // The kinds `/api/status` `network.uplink_type` reports (eth is a prefix).
  wifi: "wifi",
  cellular: "modem",
};

export const LANE_LABEL: Record<UplinkLane, string> = {
  ethernet: "Ethernet",
  wifi: "WiFi client",
  modem: "4G modem",
  usb: "USB tether",
};

/** The lane an uplink token names, or null for a token this panel has no row
 *  for (rendered raw). */
export function laneForToken(token: string | null | undefined): UplinkLane | null {
  if (!token) return null;
  const exact = LANE_BY_TOKEN[token];
  if (exact) return exact;
  // The router's live sentinel carries the kernel name (eth0, end0, enp1s0…).
  if (token.startsWith("eth") || token.startsWith("en")) return "ethernet";
  if (token.startsWith("wwan")) return "modem";
  if (token.startsWith("usb")) return "usb";
  if (token.startsWith("wlan")) return "wifi";
  return null;
}

/** The `ethernet` leg of `GET /api/v1/ground-station/network`: the board's
 *  wired port, or null when it has none. */
export interface EthernetLane {
  available?: boolean;
  iface?: string | null;
  link?: boolean | null;
  speed_mbps?: number | null;
  ip?: string | null;
  gateway?: string | null;
}

/** Up/detail for the Ethernet row. `up` is null when the agent did not report
 *  the carrier, never a fabricated "down". */
export function ethernetLaneView(eth: EthernetLane | null | undefined): { up: boolean | null; detail?: string } {
  if (!eth) return { up: null };
  const parts = [eth.ip, eth.speed_mbps != null ? `${eth.speed_mbps} Mb/s` : null].filter(Boolean);
  return { up: typeof eth.link === "boolean" ? eth.link : null, detail: parts.join(" · ") || undefined };
}

/** A human label for an uplink token, falling back to the token itself. */
export function uplinkTokenLabel(token: string): string {
  const lane = laneForToken(token);
  return lane ? LANE_LABEL[lane] : token;
}

/** The selected uplink: the lane composite's `active_uplink`, else the
 *  status payload's `network.uplink_type` (`eth`, `wifi`, `cellular`, `usb`,
 *  or an interface name). `none` and an absent reading are no uplink. */
export function activeUplinkToken(active: string | null | undefined, uplinkType: string | null | undefined): string | null {
  const token = active?.trim() || uplinkType?.trim() || null;
  return token === "none" ? null : token;
}

/** A lane's tone and whether it carries the active ring. A lane that is up
 *  reads green; the active lane is ringed, and amber only when it is degraded
 *  (the router could not reach the internet over it). A lane that is down or
 *  unread is muted. */
export function laneTone(lane: { up: boolean | null; active: boolean; reachable: boolean | null | undefined }): {
  tone: Tone;
  ring: boolean;
} {
  if (lane.active) return { tone: lane.reachable === false ? "warn" : "ok", ring: true };
  return { tone: lane.up ? "ok" : "muted", ring: false };
}
