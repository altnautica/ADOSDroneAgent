// The Hotspot settings page: what a flip of the access-point switch does.
//
// Turning the access point off is never destructive and commits at once.
// Turning it on takes over the Wi-Fi radio, so with a client link up it drops
// that link: that case waits behind a slide. With no client link it commits.

import type { WifiStatus } from "@/shared/api/types";

export type HotspotStep = { kind: "commit"; enabled: boolean } | { kind: "confirm" };

export function hotspotToggle(next: boolean, clientConnected: boolean): HotspotStep {
  if (next && clientConnected) return { kind: "confirm" };
  return { kind: "commit", enabled: next };
}

/** Only a status that says connected counts; an unknown status does not. */
export function wifiClientConnected(status: WifiStatus | null | undefined): boolean {
  return status?.connected === true;
}
