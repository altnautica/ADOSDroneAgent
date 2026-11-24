/**
 * The setup access point's network name.
 *
 * The agent resolves this itself: it reads the configured
 * `network.hotspot.ssid`, substitutes the device id into the template, and
 * publishes the result as `network.hotspot_ssid` on the status payload. That
 * is the name the radio will actually broadcast, including when an operator
 * has configured something other than the default.
 *
 * Rebuilding the name here from the device id would agree with the shipped
 * default and disagree with every configured one, and an operator who cannot
 * find the network we named is worse off than one told we do not know it. So
 * this reports what the agent published, or reports that it has nothing.
 *
 * @module lib/hotspot
 */

import type { NetworkStatus } from "../agent-api.generated";
import type { GsApLeg } from "./uplink";

export type HotspotSsid =
  | { known: true; ssid: string; iface: string | null }
  | { known: false; iface: string | null };

/** Read the access point name the agent published, if it published one.
 *
 * On a ground station the live AP leg of `/api/v1/ground-station/network`
 * (`ap`) is the authority: it names the SSID hostapd broadcasts and the
 * interface it runs on, which is not the WFB radio. It wins over the status
 * payload's name. The interface is null when no AP leg was read. */
export function resolveHotspotSsid(network?: NetworkStatus, ap?: GsApLeg | null): HotspotSsid {
  const iface = ap?.interface?.trim() || null;
  const ssid = ap?.ssid?.trim() || network?.hotspot_ssid?.trim();
  if (!ssid) return { known: false, iface };
  return { known: true, ssid, iface };
}
