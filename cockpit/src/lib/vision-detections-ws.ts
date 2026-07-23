// Live detection feed for the on-box cockpit. Mints a scoped WebSocket ticket,
// opens the agent's `/api/vision/detections/ws` bridge (the FastAPI route that
// forwards the vision engine's detection-batch broadcast socket as JSON), decodes
// each `DetectionBatch`, and pushes it into the cockpit detections store so the
// overlay draws live boxes. A browser cannot speak the engine's binary Unix
// socket, so this WebSocket bridge is the path; the decode below maps the wire's
// snake_case contract field names onto the store's camelCase shape.
//
// This is the LAN / on-box path: the cockpit is served by the same agent whose
// engine produces the detections, so the socket is same-origin and the ticket
// mint is unguarded on-box (it attaches an optional session/key only when reached
// off-box). Reconnect is bounded-backoff; a node with no vision engine simply
// yields an empty store (the route closes cleanly) so the overlay shows nothing
// rather than fabricating boxes.

import type {
  CockpitDetection,
  CockpitDetectionBatch,
  LockState,
} from "@/stores/detections-store";
import { useDetectionsStore } from "@/stores/detections-store";
import { WS_TICKET_PROTOCOL, mintWsTicket } from "@/lib/ws-ticket";

/** The scope a `/api/vision/detections/ws` ticket must be minted for. Matches the
 *  route's `_WS_SCOPE` (`vision.detections`) so the agent validates it on
 *  consume. */
export const VISION_DETECTIONS_SCOPE = "vision.detections";

/** The engine normalizes frames to this default before inference, so a batch that
 *  omits explicit frame dimensions is scaled against these. Matches the agent's
 *  `vision.downscale_width` / `downscale_height` defaults. */
const DEFAULT_FRAME_WIDTH = 640;
const DEFAULT_FRAME_HEIGHT = 480;

/** The detection-batch wire version this cockpit speaks. The agent stamps `v` on
 *  every batch (`ados_protocol::framebus::VISION_DETECTION_VERSION`) and rejects a
 *  version it does not speak; the cockpit mirrors that contract. A batch whose `v`
 *  is present but does NOT equal this is DROPPED rather than mis-mapped onto a
 *  shape a newer version may have reshaped — never present garbage as data. A
 *  batch with no `v` (a transport that omits it) maps normally. */
const SUPPORTED_DETECTION_VERSION = 2;

const RECONNECT_MIN_MS = 1000;
const RECONNECT_MAX_MS = 10_000;

/** One detection as it arrives on the wire (contract field names). The box and
 *  the richer percept fields are optional so a box-less percept, or a batch from
 *  an agent that predates a field, maps cleanly. */
interface WireDetection {
  bbox?: { x?: unknown; y?: unknown; width?: unknown; height?: unknown };
  class_label?: unknown;
  confidence?: unknown;
  track_id?: unknown;
  lock_state?: unknown;
}

/** A detection batch as the agent forwards it (contract field names). */
interface WireDetectionBatch {
  v?: unknown;
  model_id?: unknown;
  camera_id?: unknown;
  frame_id?: unknown;
  ts_ms?: unknown;
  frame_width?: unknown;
  frame_height?: unknown;
  detections?: unknown;
}

function num(v: unknown): number {
  return typeof v === "number" && Number.isFinite(v) ? v : 0;
}

function str(v: unknown): string {
  return typeof v === "string" ? v : "";
}

function lockState(v: unknown): LockState | null {
  return v === "locked" || v === "uncertain" || v === "lost" ? v : null;
}

function mapDetection(raw: WireDetection): CockpitDetection {
  const b = raw.bbox ?? {};
  const trackId =
    typeof raw.track_id === "number" && Number.isFinite(raw.track_id)
      ? raw.track_id
      : null;
  return {
    // Absent for a box-less percept (mask/pose/depth only); a box detector
    // always sends it.
    bbox:
      raw.bbox != null && typeof raw.bbox === "object"
        ? { x: num(b.x), y: num(b.y), width: num(b.width), height: num(b.height) }
        : undefined,
    classLabel: str(raw.class_label),
    confidence: num(raw.confidence),
    trackId,
    lockState: lockState(raw.lock_state),
  };
}

/**
 * Map a wire batch onto the store's camelCase shape (minus `receivedAt`, which
 * the store stamps). Returns `null` for a batch whose wire version is present but
 * NOT the one we speak: a newer version may reshape fields, so mapping it would
 * silently produce garbage. An absent `v` maps normally. Frame dimensions default
 * to the engine's normalized size unless the batch advertised them.
 */
export function mapWireBatch(
  raw: WireDetectionBatch,
): Omit<CockpitDetectionBatch, "receivedAt"> | null {
  if (raw.v !== undefined && raw.v !== SUPPORTED_DETECTION_VERSION) {
    return null;
  }
  const detections = Array.isArray(raw.detections)
    ? raw.detections.flatMap((d) =>
        d && typeof d === "object" ? [mapDetection(d as WireDetection)] : [],
      )
    : [];
  return {
    modelId: str(raw.model_id),
    cameraId: str(raw.camera_id),
    frameId: num(raw.frame_id),
    tsMs: num(raw.ts_ms),
    frameWidth: num(raw.frame_width) || DEFAULT_FRAME_WIDTH,
    frameHeight: num(raw.frame_height) || DEFAULT_FRAME_HEIGHT,
    detections,
  };
}

/**
 * Parse a detection-batch JSON string (the contract's snake_case shape the WS
 * forwards) into the store's camelCase batch, or `null` when the text is
 * malformed / not an object / a version we do not speak. Malformed payloads are
 * dropped (returns null), never thrown.
 */
export function parseWireDetectionJson(
  text: string,
): Omit<CockpitDetectionBatch, "receivedAt"> | null {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return null;
  }
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  return mapWireBatch(raw as WireDetectionBatch);
}

export interface ConnectVisionDetectionsOptions {
  /** Optional connection-state callback for surfacing link health. */
  onState?: (state: "connected" | "reconnecting" | "closed") => void;
}

/**
 * Open the live-detection WebSocket to this box's own agent and feed the store.
 * Returns a cleanup function that closes the socket and clears the store so a
 * stale box set does not linger. Bounded-backoff reconnect; a node with no vision
 * engine has the route close cleanly, so the store stays empty (no boxes) rather
 * than churning fabricated state.
 */
export function connectVisionDetections(
  opts: ConnectVisionDetectionsOptions = {},
): () => void {
  if (typeof window === "undefined") return () => {};
  const { onState } = opts;

  const setBatch = useDetectionsStore.getState().setBatch;
  const clearBatch = useDetectionsStore.getState().clear;

  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const url = `${proto}//${location.host}/api/vision/detections/ws`;

  let closed = false;
  let socket: WebSocket | null = null;
  let reconnectMs = RECONNECT_MIN_MS;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  const controller = new AbortController();

  const scheduleReconnect = () => {
    if (closed || reconnectTimer) return;
    onState?.("reconnecting");
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      reconnectMs = Math.min(reconnectMs * 2, RECONNECT_MAX_MS);
      void connect();
    }, reconnectMs);
  };

  const connect = async () => {
    if (closed) return;
    const ticket = await mintWsTicket(VISION_DETECTIONS_SCOPE, controller.signal);
    if (closed) return;

    socket = ticket
      ? new WebSocket(url, [WS_TICKET_PROTOCOL, ticket])
      : new WebSocket(url);

    socket.onopen = () => {
      reconnectMs = RECONNECT_MIN_MS;
      onState?.("connected");
    };

    socket.onmessage = (msg) => {
      const batch = parseWireDetectionJson(
        typeof msg.data === "string" ? msg.data : "",
      );
      // A version this cockpit does not speak maps to null — skip it, never feed
      // the store a mis-mapped batch.
      if (batch) setBatch(batch);
    };

    socket.onclose = () => {
      socket = null;
      scheduleReconnect();
    };

    socket.onerror = () => {
      // onclose fires next and owns the reconnect.
      try {
        socket?.close();
      } catch {
        // already closing
      }
    };
  };

  void connect();

  return () => {
    closed = true;
    controller.abort();
    if (reconnectTimer) clearTimeout(reconnectTimer);
    if (socket) {
      socket.onclose = null;
      try {
        socket.close();
      } catch {
        // ignore
      }
      socket = null;
    }
    clearBatch();
    onState?.("closed");
  };
}
