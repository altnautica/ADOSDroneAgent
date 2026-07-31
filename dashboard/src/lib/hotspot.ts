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

import type { NetworkInfo } from "./types";

export type HotspotSsid =
  | { known: true; ssid: string }
  | { known: false };

/** Read the access point name the agent published, if it published one. */
export function resolveHotspotSsid(network?: NetworkInfo): HotspotSsid {
  const ssid = network?.hotspot_ssid?.trim();
  if (!ssid) return { known: false };
  return { known: true, ssid };
}
