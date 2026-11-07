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
 * into the mutable layer frame store, outside React. True when consumed.
 */
export function ingestLayerFrame(raw: unknown): boolean {
  if (typeof raw !== "object" || raw === null) return false;
  const f = raw as { kind?: unknown; data?: unknown };
  switch (f.kind) {
    case "ar": {
      const d = parseArFrame(f.data);
      if (d) layerFrames.putAr(d);
      return true;
    }
    case "obstacles": {
      const d = parseObstaclesFrame(f.data);
      if (d) layerFrames.putObstacles(d);
      return true;
    }
    case "depth": {
      const d = parseDepthFrame(f.data);
      if (d) layerFrames.putDepth(d);
      return true;
    }
    case "surfaces": {
      const d = parseSurfacesFrame(f.data);
      if (d) layerFrames.putSurfaces(d);
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
      store.setLink("connected");
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

  return () => {
    closed = true;
    controller.abort();
    clearInterval(poll);
    clearInterval(leaseTimer);
    leaseSocket = null;
    inFlight?.abort();
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
