// The ground-station uplink lanes, keyed by the tokens the agent's uplink
// router actually reports: `active_uplink` and every `priority` entry are
// interface-role names (eth0 / wlan0_client / wwan0 / usb0), not lane words.

export type UplinkLane = "ethernet" | "wifi" | "modem" | "usb";

const LANE_BY_TOKEN: Record<string, UplinkLane> = {
  eth0: "ethernet",
  wlan0_client: "wifi",
  wwan0: "modem",
  usb0: "usb",
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
  return token ? (LANE_BY_TOKEN[token] ?? null) : null;
}

/** A human label for an uplink token, falling back to the token itself. */
export function uplinkTokenLabel(token: string): string {
  const lane = laneForToken(token);
  return lane ? LANE_LABEL[lane] : token;
}
