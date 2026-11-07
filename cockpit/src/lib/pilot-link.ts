// The operator link to the drone's pilot: `WS /api/v1/pilot/stream` (ticket
// scope `pilot.control`). One socket per cockpit, opened while the Feed is
// mounted on a node that flies. It carries:
//   - stick `input` frames (50 Hz while a controller or the on-screen sticks
//     stream), `release` when they stop, and `session.take` to move the single
//     stick session to this surface;
//   - pilot `action`s (pause, take_control, launch, profile:…), each answered
//     by an `action_result` matched on its id;
//   - a 1 Hz `hb` while no input streams, which is the surface heartbeat the
//     pilot's link-loss failsafe watches.
// The server pushes the `pilot` state at 10 Hz; it lands in the nav-stream
// store through the same parser the nav stream uses, so every reader sees one
// pilot state whichever socket delivered it.

import { create } from "zustand";

import { parseNavFrame } from "@/lib/nav-stream";
import { ReconnectBackoff } from "@/lib/reconnect-backoff";
import { WS_TICKET_PROTOCOL, mintWsTicket } from "@/shared/ws-ticket";
import { useNavStreamStore } from "@/stores/nav-stream-store";

export const PILOT_STREAM_SCOPE = "pilot.control";
export const PILOT_STREAM_PATH = "/api/v1/pilot/stream";
/** Heartbeat interval while no input streams. */
export const PILOT_HB_MS = 1000;
/** An action with no result after this long reports a timeout. */
export const ACTION_TIMEOUT_MS = 3000;
/** The server keeps alive every second; silent this long means redial. */
const PILOT_SILENT_MS = 5000;

/** `[roll, pitch, throttle, yaw]`, each in [-1, 1], deadzone applied. */
export type PilotAxes = readonly [number, number, number, number];

export type PilotSource = "cockpit" | "kiosk";

export interface ActionResult {
  ok: boolean;
  err?: string;
}

export type PilotLinkState = "closed" | "connecting" | "connected";

interface PilotLinkStore {
  link: PilotLinkState;
  /** True while this surface streams stick input. */
  streaming: boolean;
  setLink: (link: PilotLinkState) => void;
  setStreaming: (streaming: boolean) => void;
}

export const usePilotLinkStore = create<PilotLinkStore>((set) => ({
  link: "closed",
  streaming: false,
  setLink: (link) => set({ link }),
  setStreaming: (streaming) => set({ streaming }),
}));

function newSessionId(): number {
  const word = new Uint32Array(1);
  crypto.getRandomValues(word);
  // Never 0: the pilot reads 0 as "no session".
  return word[0] === 0 ? 1 : word[0];
}

/** This surface's stick session id (a u32), fixed for the page's lifetime. */
export const PILOT_SESSION: number = typeof crypto !== "undefined" ? newSessionId() : 1;

let socket: WebSocket | null = null;
let src: PilotSource = "cockpit";
let seq = 0;
let actionSeq = 0;
const pendingActions = new Map<string, { resolve: (r: ActionResult) => void; timer: ReturnType<typeof setTimeout> }>();

function send(frame: Record<string, unknown>): boolean {
  if (!socket || socket.readyState !== WebSocket.OPEN) return false;
  socket.send(JSON.stringify(frame));
  return true;
}

/** Stream one stick sample. Returns false when the link is not open. */
export function sendPilotInput(axes: PilotAxes, buttons: number): boolean {
  const ok = send({
    op: "input",
    src,
    session: PILOT_SESSION,
    seq: ++seq,
    t_ms: Date.now(),
    axes: [axes[0], axes[1], axes[2], axes[3]],
    buttons,
  });
  if (ok && !usePilotLinkStore.getState().streaming) usePilotLinkStore.getState().setStreaming(true);
  return ok;
}

/** Stop streaming: the pilot brakes to a hold. */
export function releasePilotInput(): void {
  if (usePilotLinkStore.getState().streaming) usePilotLinkStore.getState().setStreaming(false);
  send({ op: "release", src, session: PILOT_SESSION });
}

/** Move the single stick session to this surface. */
export function takePilotSession(): boolean {
  return send({ op: "session.take", src, session: PILOT_SESSION });
}

/** Send a pilot action and resolve with its result (or a timeout / no-link
 *  failure; never rejects). */
export function sendPilotAction(name: string, args?: Record<string, unknown>): Promise<ActionResult> {
  const id = `${PILOT_SESSION}-${++actionSeq}`;
  const frame: Record<string, unknown> = { op: "action", name, src, id };
  if (args) frame.args = args;
  if (!send(frame)) return Promise.resolve({ ok: false, err: "no pilot link" });
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      pendingActions.delete(id);
      resolve({ ok: false, err: "no response" });
    }, ACTION_TIMEOUT_MS);
    pendingActions.set(id, { resolve, timer });
  });
}

/** Handle one server frame: pilot state into the nav-stream store, action
 *  results to their waiting caller. Exported for tests. */
export function handlePilotFrame(raw: unknown): void {
  if (typeof raw !== "object" || raw === null) return;
  const f = raw as { kind?: unknown; id?: unknown; ok?: unknown; err?: unknown };
  if (f.kind === "action_result") {
    if (typeof f.id !== "string") return;
    const waiter = pendingActions.get(f.id);
    if (!waiter) return;
    pendingActions.delete(f.id);
    clearTimeout(waiter.timer);
    waiter.resolve({ ok: f.ok === true, err: typeof f.err === "string" ? f.err : undefined });
    return;
  }
  const frame = parseNavFrame(raw);
  if (frame) useNavStreamStore.getState().ingest(frame);
}

/** Open the pilot link. Returns a cleanup that releases input and closes. */
export function connectPilotLink(source: PilotSource): () => void {
  if (typeof window === "undefined") return () => {};
  src = source;
  const store = usePilotLinkStore.getState();
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  const url = `${proto}//${location.host}${PILOT_STREAM_PATH}`;
  const backoff = new ReconnectBackoff(1000, 10_000);
  const controller = new AbortController();
  let closed = false;
  let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  let silentTimer: ReturnType<typeof setTimeout> | null = null;

  const scheduleReconnect = () => {
    if (closed || reconnectTimer) return;
    store.setLink("connecting");
    reconnectTimer = setTimeout(() => {
      reconnectTimer = null;
      void connect();
    }, backoff.next());
  };
  const armSilence = () => {
    clearTimeout(silentTimer ?? undefined);
    silentTimer = setTimeout(() => socket?.close(), PILOT_SILENT_MS);
  };

  const connect = async () => {
    if (closed) return;
    store.setLink("connecting");
    const ticket = await mintWsTicket(PILOT_STREAM_SCOPE, controller.signal);
    if (closed) return;
    try {
      socket = ticket ? new WebSocket(url, [WS_TICKET_PROTOCOL, ticket]) : new WebSocket(url);
    } catch {
      socket = null;
      scheduleReconnect();
      return;
    }
    socket.onopen = () => {
      backoff.opened();
      armSilence();
      store.setLink("connected");
    };
    socket.onmessage = (msg) => {
      backoff.message();
      armSilence();
      try {
        handlePilotFrame(JSON.parse(typeof msg.data === "string" ? msg.data : ""));
      } catch {
        // malformed frame: ignored
      }
    };
    socket.onclose = () => {
      socket = null;
      clearTimeout(silentTimer ?? undefined);
      usePilotLinkStore.getState().setStreaming(false);
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

  const hb = setInterval(() => {
    if (!usePilotLinkStore.getState().streaming) send({ op: "hb", src });
  }, PILOT_HB_MS);

  void connect();

  return () => {
    closed = true;
    controller.abort();
    clearInterval(hb);
    clearTimeout(reconnectTimer ?? undefined);
    clearTimeout(silentTimer ?? undefined);
    releasePilotInput();
    for (const [id, w] of pendingActions) {
      clearTimeout(w.timer);
      w.resolve({ ok: false, err: "closed" });
      pendingActions.delete(id);
    }
    if (socket) {
      socket.onclose = null;
      try {
        socket.close();
      } catch {
        // ignore
      }
      socket = null;
    }
    usePilotLinkStore.getState().setLink("closed");
  };
}
