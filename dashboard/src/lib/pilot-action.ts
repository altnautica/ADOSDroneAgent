// Pilot actions from the dashboard: the Home countdown card's buttons (Cancel
// on a low-battery return; Skip waypoint, Return and Land here on a mission
// stall) over `WS /api/v1/pilot/stream` (ticket scope `pilot.control`), the
// route the cockpit flies on. Each action is answered by an `action_result`
// matched on its id.
//
// The link opens on the first action and then stays, with a 1 Hz `hb`, while
// the page lives (redialled if it drops). The pilot counts an action as an
// operator surface speaking and from then on watches that surface for link
// loss, so a surface that acted must keep answering rather than fall silent
// after one frame. State frames the server pushes are ignored: the dashboard
// reads the pilot from the nav-status poll.
//
// Every frame carries the page's instance id (`sid`), kept across redials, so
// the pilot watches each open dashboard's link on its own; the page says
// `bye` when it goes away.

import { mintWsTicket, ticketProtocols } from "@/shared/ws-ticket";

export const PILOT_STREAM_SCOPE = "pilot.control";
export const PILOT_STREAM_PATH = "/api/v1/pilot/stream";
/** The surface name the pilot sees on every frame. */
export const PILOT_SRC = "dashboard";
/** Heartbeat interval while the link is open. */
export const PILOT_HB_MS = 1000;
/** An action with no result after this long reports a timeout; the same
 *  window bounds waiting for the link to open. */
export const ACTION_TIMEOUT_MS = 3000;
/** Wait before redialling a dropped link. */
export const REDIAL_MS = 2000;

export interface ActionResult {
  ok: boolean;
  err?: string;
}

/** The part of a browser WebSocket the link uses. */
export interface SocketLike {
  readonly readyState: number;
  send(data: string): void;
  close(): void;
  onopen: (() => void) | null;
  onmessage: ((e: { data: unknown }) => void) | null;
  onclose: (() => void) | null;
  onerror: (() => void) | null;
}

export interface PilotLinkDeps {
  dial: (url: string, protocols: string[]) => SocketLike;
  ticket: () => Promise<string | null>;
  url: () => string;
}

const OPEN = 1;

/** A surface instance id: 32 hex. */
export function newInstanceId(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/** One pilot link for the page. */
export class PilotActionLink {
  private socket: SocketLike | null = null;
  private opening: Promise<SocketLike | null> | null = null;
  private hb: ReturnType<typeof setInterval> | null = null;
  private redial: ReturnType<typeof setTimeout> | null = null;
  private seq = 0;
  private closed = false;
  private readonly pending = new Map<string, { resolve: (r: ActionResult) => void; timer: ReturnType<typeof setTimeout> }>();
  /** This link's surface instance id. */
  readonly sid = newInstanceId();

  constructor(private readonly deps: PilotLinkDeps) {}

  /** Send a pilot action and resolve with its result, a timeout or a
   *  no-link failure; never rejects. */
  async send(name: string): Promise<ActionResult> {
    const socket = await this.open();
    if (!socket || socket.readyState !== OPEN) return { ok: false, err: "no pilot link" };
    const id = `${PILOT_SRC}-${++this.seq}`;
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        resolve({ ok: false, err: "no response" });
      }, ACTION_TIMEOUT_MS);
      this.pending.set(id, { resolve, timer });
      this.frame({ op: "action", name, id });
    });
  }

  /** Close the link for good, saying `bye`: pending actions answer "no
   *  pilot link". */
  close(): void {
    this.closed = true;
    this.frame({ op: "bye" });
    clearInterval(this.hb ?? undefined);
    clearTimeout(this.redial ?? undefined);
    this.socket?.close();
    this.socket = null;
    this.failPending("no pilot link");
  }

  /** Send one frame with this surface's name and instance id, if open. */
  private frame(f: Record<string, unknown>): void {
    if (this.socket?.readyState === OPEN) this.socket.send(JSON.stringify({ ...f, src: PILOT_SRC, sid: this.sid }));
  }

  /** The open socket, dialling it when there is none. */
  private open(): Promise<SocketLike | null> {
    if (this.closed) return Promise.resolve(null);
    if (this.socket?.readyState === OPEN) return Promise.resolve(this.socket);
    this.opening ??= this.dial().finally(() => {
      this.opening = null;
    });
    return this.opening;
  }

  private async dial(): Promise<SocketLike | null> {
    const ticket = await this.deps.ticket();
    if (this.closed) return null;
    let socket: SocketLike;
    try {
      socket = this.deps.dial(this.deps.url(), ticketProtocols(ticket));
    } catch {
      return null;
    }
    this.socket = socket;
    socket.onmessage = (e) => this.onMessage(e.data);
    socket.onerror = () => socket.close();
    socket.onclose = () => {
      if (this.socket !== socket) return;
      this.socket = null;
      clearInterval(this.hb ?? undefined);
      this.hb = null;
      this.failPending("no pilot link");
      // Once this surface has acted it keeps its link.
      if (!this.closed) this.redial = setTimeout(() => void this.open(), REDIAL_MS);
    };
    return new Promise((resolve) => {
      const timer = setTimeout(() => resolve(null), ACTION_TIMEOUT_MS);
      socket.onopen = () => {
        clearTimeout(timer);
        clearInterval(this.hb ?? undefined);
        this.hb = setInterval(() => this.frame({ op: "hb" }), PILOT_HB_MS);
        resolve(socket);
      };
    });
  }

  private onMessage(data: unknown): void {
    let f: { kind?: unknown; id?: unknown; ok?: unknown; err?: unknown };
    try {
      f = JSON.parse(typeof data === "string" ? data : "") as typeof f;
    } catch {
      return;
    }
    if (f?.kind !== "action_result" || typeof f.id !== "string") return;
    const waiter = this.pending.get(f.id);
    if (!waiter) return;
    this.pending.delete(f.id);
    clearTimeout(waiter.timer);
    waiter.resolve(f.ok === true ? { ok: true } : { ok: false, err: typeof f.err === "string" ? f.err : "refused" });
  }

  private failPending(err: string): void {
    for (const [id, w] of this.pending) {
      clearTimeout(w.timer);
      w.resolve({ ok: false, err });
      this.pending.delete(id);
    }
  }
}

let link: PilotActionLink | null = null;

/** Send a pilot action over the page's pilot link (opened on first use). */
export function sendPilotAction(name: string): Promise<ActionResult> {
  if (!link) {
    link = new PilotActionLink({
      dial: (url, protocols) => new WebSocket(url, protocols) as unknown as SocketLike,
      ticket: () => mintWsTicket(PILOT_STREAM_SCOPE),
      url: () => `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}${PILOT_STREAM_PATH}`,
    });
    const page = link;
    // Closed for good; a page restored from the back-forward cache dials anew.
    window.addEventListener(
      "pagehide",
      () => {
        page.close();
        if (link === page) link = null;
      },
      { once: true },
    );
  }
  return link.send(name);
}
