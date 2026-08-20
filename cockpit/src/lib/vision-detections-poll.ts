// Ground-station detection poll for the on-box cockpit.
//
// A ground node has no local vision engine — the boxes it shows come over the
// radio from the linked drone. The drone's own `/vision/detections/latest`
// (which replays its most-recent batch from the same broadcast socket the WS
// route streams) is reached through the ground agent's unary relay-proxy
// route, which tunnels one HTTP request/response pair over the aux lane. This
// polls that relay route and feeds the cockpit's detection store through the
// SAME `mapWireBatch` the LAN WebSocket path uses, so the overlay draws boxes
// identically whether the source is a local engine or a relayed one.
//
// Polling is the designed cadence here: the relay proxy is unary (one
// request/response pair; a WebSocket cannot cross it), so a short poll is the
// honest stand-in for a stream. A silent or absent peer yields no new batch,
// and the overlay ages stale boxes out via `DETECTION_STALE_MS` rather than
// pinning them — a ground node with no link shows clean no-signal, never
// fabricated boxes.

import { apiFetch } from "@/lib/api";
import { mapWireBatch } from "@/lib/vision-detections-ws";
import { useDetectionsStore } from "@/stores/detections-store";

/** Poll cadence. ~4 Hz is comfortably below the engine's batch rate but fast
 *  enough that a fresh box lands within a human reaction time; the relay round
 *  trip over the radio is bounded by the proxy's own timeout. */
const POLL_INTERVAL_MS = 250;

export interface ConnectGroundDetectionPollOptions {
  /** The linked drone's device id, used as the relay-proxy peer. */
  peer: string;
  intervalMs?: number;
}

/**
 * Begin polling the linked drone's latest detection batch over the relay proxy
 * and feeding it into the detection store. Returns a stop function that
 * cancels the poll and clears the store's boxes (so a stale feed never pins
 * the last frame's boxes once the cockpit leaves the flying view).
 */
export function connectGroundDetectionPoll(
  opts: ConnectGroundDetectionPollOptions,
): () => void {
  const { peer, intervalMs = POLL_INTERVAL_MS } = opts;
  const url = `/api/v1/ground-station/relay-proxy/${encodeURIComponent(
    peer,
  )}/vision/detections/latest`;

  let cancelled = false;
  let timer: ReturnType<typeof setTimeout> | null = null;

  const tick = async () => {
    if (cancelled) return;
    try {
      const raw = await apiFetch<unknown>(url);
      const mapped = mapWireBatch(raw as never);
      if (mapped) useDetectionsStore.getState().setBatch(mapped);
    } catch {
      // A relayed drone that is silent — radio down, proxy not initialised, or
      // the drone's vision idle — yields no new batch. The overlay ages the
      // last boxes out on its own window; nothing is fabricated.
    } finally {
      if (!cancelled) timer = setTimeout(tick, intervalMs);
    }
  };

  void tick();

  return () => {
    cancelled = true;
    if (timer) clearTimeout(timer);
    useDetectionsStore.getState().clear();
  };
}
