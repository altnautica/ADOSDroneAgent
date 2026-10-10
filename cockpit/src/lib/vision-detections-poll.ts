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

import { apiFetch } from "@/shared/api-fetch";
import { mapWireBatch } from "@/lib/vision-detections-ws";
import { useDetectionsStore } from "@/stores/detections-store";

/** Cadence while the drone is producing new batches: ~4 Hz lands a fresh box
 *  within a human reaction time. */
export const DETECTION_ACTIVE_MS = 250;
/** Cadence while the drone reports nothing new (vision idle, radio quiet):
 *  a slow probe, so an idle engine does not cost the radio four relayed
 *  requests a second. The first new batch switches straight back. */
export const DETECTION_IDLE_MS = 5000;

export interface ConnectGroundDetectionPollOptions {
  /** The linked drone's device id, used as the relay-proxy peer. */
  peer: string;
}

/** The next poll delay: fast while a new frame arrived, slow otherwise. */
export function nextDetectionDelay(gotNewFrame: boolean): number {
  return gotNewFrame ? DETECTION_ACTIVE_MS : DETECTION_IDLE_MS;
}

/**
 * Begin polling the linked drone's latest detection batch over the relay proxy
 * and feeding it into the detection store. Returns a stop function that
 * cancels the poll and clears the store's boxes (so a stale feed never pins
 * the last frame's boxes once the cockpit leaves the flying view).
 */
export function connectGroundDetectionPoll(opts: ConnectGroundDetectionPollOptions): () => void {
  const url = `/api/v1/ground-station/relay-proxy/${encodeURIComponent(
    opts.peer,
  )}/vision/detections/latest`;

  let cancelled = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let lastFrame: string | null = null;

  const tick = async () => {
    if (cancelled) return;
    let fresh = false;
    try {
      const mapped = mapWireBatch((await apiFetch<unknown>(url)) as never);
      if (mapped) {
        const key = `${mapped.cameraId ?? ""}:${mapped.frameId}`;
        fresh = key !== lastFrame;
        lastFrame = key;
        if (fresh) useDetectionsStore.getState().setBatch(mapped);
      }
    } catch {
      // A silent relayed drone yields no new batch; the overlay ages the last
      // boxes out on its own window. Nothing is fabricated.
    }
    if (!cancelled) timer = setTimeout(tick, nextDetectionDelay(fresh));
  };

  void tick();

  return () => {
    cancelled = true;
    clearTimeout(timer);
    useDetectionsStore.getState().clear();
  };
}
