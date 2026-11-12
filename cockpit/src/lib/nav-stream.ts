// The live navigation feed for this node's Feed. Opens the agent's
// `WS /api/v1/nav/stream` (ticket scope `nav.stream`, minted exactly like the
// detection stream so no credential reaches the URL), parses each frame into
// the nav-stream store, and polls `GET /api/v1/map/live?since=<seq>` at 1 Hz
// for changed occupancy tiles, carrying the last `seq` forward.
//
// Frames: `{kind: "pose"|"scan"|"nav"|"follow"|"map", ts_ms, data}` carry the
// source payload verbatim; `{kind: "follow_event", topic, data}` carries a
// discrete `follow.reacquired` / `follow.lost`; `{kind: "keepalive"}` only
// proves the socket is alive. A node with nothing publishing simply leaves the
// store empty, and every surface reading it shows its absent state.
//
// When the stream has been down for 5 s (a radio relay that carries HTTP but
// not the socket), `GET /api/v1/nav/status` is polled at 2 Hz instead and the
// store reports the reduced rate (`radioFallback`) until the stream returns.

import { ReconnectBackoff } from "@/lib/reconnect-backoff";
import { layerFrames } from "@/lib/layer-frames";
import { apiFetch } from "@/shared/api-fetch";
import {
  parseArFrame,
  parseDepthFrame,
  parseObstaclesFrame,
  parsePilotState,
  parseSurfacesFrame,
} from "@/shared/hud/cockpit-frames";
import {
  parseFollowEvent,
  parseFollowState,
  parseMapStatus,
  parseNavPose,
  parseNavScan,
  parseNavState,
  parseOccupancyLive,
} from "@/shared/hud/nav-types";
import { WS_TICKET_PROTOCOL, mintWsTicket } from "@/shared/ws-ticket";
import { useNavStreamStore, type NavFrame } from "@/stores/nav-stream-store";

/** The scope a `/api/v1/nav/stream` ticket is minted for. */
export const NAV_STREAM_SCOPE = "nav.stream";
/** Interval of the occupancy poll. */
export const MAP_LIVE_POLL_MS = 1000;
/** The server keeps alive every 5 s; a socket silent this long is redialled. */
export const NAV_SILENT_MS = 15_000;
/** Interval of the `GET /api/v1/nav/status` fallback poll (2 Hz). */
export const NAV_STATUS_POLL_MS = 500;
/** How long the stream may stay down before the status poll takes over. */
export const NAV_STREAM_FALLBACK_MS = 5000;
/** A status read not answered within this is aborted (and counts as failed):
 *  long enough for a slow radio link to answer, short enough that a dead
 *  link reads stale within the pilot's 3 s link threshold. */
export const NAV_STATUS_TIMEOUT_MS = 2500;

const RECONNECT_MIN_MS = 1000;
const RECONNECT_MAX_MS = 10_000;

/** The nav stream's view lease: the camera and layers on screen. The agent
 *  computes `ar`/`obstacles`/`depth`/`surfaces` per camera only while some
 *  client leases them; a lease lapses 6 s after its last renewal. */
export interface ViewLease {
  cameraId: string;
  layers: string[];
}
/** Lease renewal period. */
export const VIEW_LEASE_RENEW_MS = 2000;

let viewLease: ViewLease | null = null;
let leaseSocket: WebSocket | null = null;

function sendViewLease(): void {
  if (!leaseSocket || leaseSocket.readyState !== WebSocket.OPEN) return;
  // A null lease is sent as an empty layer set, which drops this client's demand.
  const msg = viewLease
    ? { type: "view", camera_id: viewLease.cameraId, layers: viewLease.layers }
    : { type: "view", camera_id: "", layers: [] };
  try {
    leaseSocket.send(JSON.stringify(msg));
  } catch {
    // closing; the next open re-sends
  }
}

/** Set (or with null, drop) the view lease; sent at once when it changed. */
export function setViewLease(lease: ViewLease | null): void {
  const same =
    lease === viewLease ||
    (lease !== null &&
      viewLease !== null &&
      lease.cameraId === viewLease.cameraId &&
      lease.layers.join(",") === viewLease.layers.join(","));
  viewLease = lease;
  if (!same) sendViewLease();
}

/** Parse one nav-stream frame; null for keepalives, unknown kinds and
 *  malformed payloads (never thrown). */
export function parseNavFrame(raw: unknown): NavFrame | null {
  if (typeof raw !== "object" || raw === null) return null;
  const f = raw as { kind?: unknown; data?: unknown; topic?: unknown };
  switch (f.kind) {
    case "pose": {
      const data = parseNavPose(f.data);
      return data ? { kind: "pose", data } : null;
    }
    case "scan": {
      const data = parseNavScan(f.data);
      return data ? { kind: "scan", data } : null;
    }
    case "nav": {
      const data = parseNavState(f.data);
      return data ? { kind: "nav", data } : null;
    }
    case "follow": {
      const data = parseFollowState(f.data);
      return data ? { kind: "follow", data } : null;
    }
    case "map": {
      const data = parseMapStatus(f.data);
      return data ? { kind: "map", data } : null;
    }
    case "follow_event": {
      const data = parseFollowEvent(f.topic, f.data);
      return data ? { kind: "follow_event", data } : null;
    }
    case "pilot": {
      const data = parsePilotState(f.data);
      return data ? { kind: "pilot", data } : null;
    }
    default:
      return null;
  }
}

/**
 * Route one per-frame layer kind (`ar`, `obstacles`, `depth`, `surfaces`)
 * into the mutable layer frame store, outside React. `at` is when the frame
 * was produced on the performance clock. True when consumed.
 */
export function ingestLayerFrame(raw: unknown, at = performance.now()): boolean {
  if (typeof raw !== "object" || raw === null) return false;
  const f = raw as { kind?: unknown; data?: unknown };
  switch (f.kind) {
    case "ar": {
      const d = parseArFrame(f.data);
      if (d) layerFrames.putAr(d, at);
      return true;
    }
    case "obstacles": {
      const d = parseObstaclesFrame(f.data);
      if (d) layerFrames.putObstacles(d, at);
      return true;
    }
    case "depth": {
      const d = parseDepthFrame(f.data);
      if (d) layerFrames.putDepth(d, at);
      return true;
    }
    case "surfaces": {
      const d = parseSurfacesFrame(f.data);
      if (d) layerFrames.putSurfaces(d, at);
      return true;
    }
    default:
      return false;
  }
}

/** Fetch occupancy tiles changed since the stored `seq` and merge them.
 *  Resolves false on any failure (the next poll retries). */
export async function pollMapLive(signal?: AbortSignal): Promise<boolean> {
  const since = useNavStreamStore.getState().occupancy.seq;
  try {
    const live = parseOccupancyLive(await apiFetch<unknown>(`/api/v1/map/live?since=${since}`, { signal }));
    if (!live) return false;
    useNavStreamStore.getState().mergeOccupancy(live);
    return true;
  } catch {
    return false;
  }
}

/**
 * One fallback read of `GET /api/v1/nav/status`: every kind present is
 * ingested as if it had arrived on the stream, stamped with its age on the
 * drone (`age_ms`) so a stale value never reads as fresh. The `?view=` query
 * carries the view lease, so the drone computes the same overlays it would
 * for a stream client. Resolves false on any failure, and ingests nothing
 * once `signal` is aborted.
 */
export async function pollNavStatus(signal?: AbortSignal, now: () => number = Date.now): Promise<boolean> {
  const view = viewLease && viewLease.layers.length > 0 ? `${viewLease.cameraId}:${viewLease.layers.join(",")}` : null;
  const path = view ? `/api/v1/nav/status?view=${encodeURIComponent(view)}` : "/api/v1/nav/status";
  try {
    const raw = await apiFetch<unknown>(path, { signal });
    if (signal?.aborted || typeof raw !== "object" || raw === null) return false;
    const perfNow = performance.now();
    for (const [kind, entry] of Object.entries(raw as Record<string, unknown>)) {
      if (kind === "view_demand" || typeof entry !== "object" || entry === null) continue;
      const e = entry as { ts_ms?: unknown; age_ms?: unknown; data?: unknown };
      const frame = { kind, ts_ms: e.ts_ms, data: e.data };
      const age = typeof e.age_ms === "number" && e.age_ms >= 0 ? e.age_ms : 0;
      // A layer frame keeps the time it was produced, so a kept frame
      // re-served with a growing age never refreshes its staleness clock.
      if (ingestLayerFrame(frame, perfNow - age)) continue;
      const parsed = parseNavFrame(frame);
      if (!parsed) continue;
      useNavStreamStore.getState().ingest(parsed, now() - age);
    }
    return true;
  } catch {
    return false;
  }
}

/**
 * Open the nav stream and the occupancy poll for this node. Returns a cleanup
 * that closes both and clears the store, so nothing stale lingers.
 */
export function connectNavStream(): () => void {
  if (typeof window === "undefined") return () => {};
  const store = useNavStreamStore.getState();
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const url = `${proto}//${location.host}/api/v1/nav/stream`;

  let closed = false;
  let socket: WebSocket | null = null;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let silentTimer: ReturnType<typeof setTimeout> | null = null;
  const backoff = new ReconnectBackoff(RECONNECT_MIN_MS, RECONNECT_MAX_MS);
  const controller = new AbortController();
  // Whether the stream is open, and since when it has been down (from the
  // start until its first open): the status poll takes over after
  // NAV_STREAM_FALLBACK_MS down.
  let streamUp = false;
  let downSince = Date.now();

  const scheduleReconnect = () => {
    if (closed || reconnectTimer) return;
    store.setLink("reconnecting");
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      void connect();
    }, backoff.next());
  };

  // A socket the peer stopped keeping alive is closed and redialled.
  const armSilence = () => {
    clearTimeout(silentTimer ?? undefined);
    silentTimer = setTimeout(() => socket?.close(), NAV_SILENT_MS);
  };

  const connect = async () => {
    if (closed) return;
    const ticket = await mintWsTicket(NAV_STREAM_SCOPE, controller.signal);
    if (closed) return;
    // The constructor throws synchronously on a bad subprotocol token, before
    // `onclose` exists; catching keeps the reconnect ladder alive.
    try {
      socket = ticket ? new WebSocket(url, [WS_TICKET_PROTOCOL, ticket]) : new WebSocket(url);
    } catch {
      socket = null;
      scheduleReconnect();
      return;
    }
    socket.onopen = () => {
      leaseSocket = socket;
      sendViewLease();
      backoff.opened();
      armSilence();
      streamUp = true;
      // A late status answer would land an older frame over stream data.
      statusInFlight?.abort();
      statusInFlight = null;
      store.setLink("connected");
      store.setRadioFallback(false);
    };
    socket.onmessage = (msg) => {
      backoff.message();
      armSilence();
      let raw: unknown;
      try {
        raw = JSON.parse(typeof msg.data === "string" ? msg.data : "");
      } catch {
        return;
      }
      if (ingestLayerFrame(raw)) return;
      const frame = parseNavFrame(raw);
      if (frame) useNavStreamStore.getState().ingest(frame);
    };
    socket.onclose = () => {
      if (leaseSocket === socket) leaseSocket = null;
      socket = null;
      clearTimeout(silentTimer ?? undefined);
      if (streamUp) {
        streamUp = false;
        downSince = Date.now();
      }
      scheduleReconnect();
    };
    socket.onerror = () => {
      try {
        socket?.close();
      } catch {
        // already closing
      }
    };
  };

  void connect();

  const leaseTimer = setInterval(() => {
    if (viewLease) sendViewLease();
  }, VIEW_LEASE_RENEW_MS);

  let inFlight: AbortController | null = null;
  const poll = setInterval(() => {
    if (closed || inFlight || document.visibilityState === "hidden") return;
    const ac = new AbortController();
    inFlight = ac;
    void pollMapLive(ac.signal).finally(() => {
      if (inFlight === ac) inFlight = null;
    });
  }, MAP_LIVE_POLL_MS);

  // Reduced-rate fallback: the stream did not come up (or dropped) but the
  // HTTP path may still answer, e.g. over a ground-station radio relay.
  let statusInFlight: AbortController | null = null;
  const statusPoll = setInterval(() => {
    if (closed || streamUp || statusInFlight || document.visibilityState === "hidden") return;
    if (Date.now() - downSince < NAV_STREAM_FALLBACK_MS) return;
    const ac = new AbortController();
    statusInFlight = ac;
    const timeout = setTimeout(() => ac.abort(), NAV_STATUS_TIMEOUT_MS);
    void pollNavStatus(ac.signal)
      .then((ok) => {
        if (!closed && !streamUp) store.setRadioFallback(ok);
      })
      .finally(() => {
        clearTimeout(timeout);
        if (statusInFlight === ac) statusInFlight = null;
      });
  }, NAV_STATUS_POLL_MS);

  return () => {
    closed = true;
    controller.abort();
    clearInterval(poll);
    clearInterval(statusPoll);
    clearInterval(leaseTimer);
    leaseSocket = null;
    inFlight?.abort();
    statusInFlight?.abort();
    clearTimeout(reconnectTimer ?? undefined);
    clearTimeout(silentTimer ?? undefined);
    if (socket) {
      socket.onclose = null;
      try {
        socket.close();
      } catch {
        // ignore
      }
      socket = null;
    }
    useNavStreamStore.getState().clear();
    useNavStreamStore.getState().setLink("closed");
    layerFrames.clear();
  };
}
