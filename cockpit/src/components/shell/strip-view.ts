// Pure reads behind the status strip, kept out of the component so they are
// unit-testable.

import type { GsStatus } from "@/lib/types";

/** Verdicts under which this radio decoded nothing, so loss, SNR and rate are
 *  not measured. A number there would read as a perfect link. */
const UNMEASURED_DIAG: Record<string, true> = { deaf: true, no_rf: true, no_peer: true };

/** Whether the link counters describe real decoded traffic. */
export function linkMeasured(link: GsStatus["link"] | null | undefined): boolean {
  if (!link) return false;
  if (link.link_diag && UNMEASURED_DIAG[link.link_diag]) return false;
  return link.packets_received > 0;
}

/** Paired reads from the bound key, not from whether the drone's id is known:
 *  a key bound before the peer id was learned is still a pair. */
export function stripPaired(status: GsStatus | null | undefined): boolean {
  const drone = status?.paired_drone;
  return Boolean(drone?.key_fingerprint || drone?.device_id);
}
