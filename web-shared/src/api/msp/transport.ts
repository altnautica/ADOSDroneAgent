/**
 * WebSocket byte transport to the agent's transparent MAVLink/MSP proxy
 * (`ws://<host>:8765/`). The proxy is byte-transparent for both MAVLink and
 * MSP, so this carries the browser MSP codec straight to the flight controller.
 *
 * @module lib/msp/transport
 * @license GPL-3.0-only
 */

type TransportEventMap = {
  data: Uint8Array;
  close: void;
  error: Error;
};

export class WebSocketTransport {
  readonly type = "websocket" as const;

  private ws: WebSocket | null = null;
  private _connected = false;
  private _disconnecting = false;
  private listeners: Map<keyof TransportEventMap, Set<(data: never) => void>> = new Map();

  get isConnected(): boolean {
    return this._connected;
  }

  /**
   * Connect to a WebSocket endpoint.
   * @param url — WebSocket URL, e.g. "ws://host:8765/"
   * @param protocols — optional subprotocol(s); the agent's authenticated
   *   MAVLink proxy gates the upgrade on a ticket carried as a subprotocol
   *   value. Omitting it preserves the open-posture dial for an unpaired agent.
   */
  async connect(url: string, protocols?: string | string[]): Promise<void> {
    if (this._connected) throw new Error("Already connected");

    return new Promise<void>((resolve, reject) => {
      try {
        this.ws = protocols !== undefined ? new WebSocket(url, protocols) : new WebSocket(url);
        this.ws.binaryType = "arraybuffer";
      } catch (err) {
        reject(err);
        return;
      }

      this.ws.onopen = () => {
        this._connected = true;
        resolve();
      };

      this.ws.onerror = (ev) => {
        const error = new Error(
          "WebSocket error" + ("message" in ev ? `: ${(ev as ErrorEvent).message}` : ""),
        );
        if (!this._connected) reject(error);
        else this.emit("error", error);
      };

      this.ws.onmessage = (ev: MessageEvent) => {
        if (ev.data instanceof ArrayBuffer) {
          this.emit("data", new Uint8Array(ev.data));
        }
      };

      this.ws.onclose = () => {
        const wasConnected = this._connected;
        this._connected = false;
        this.ws = null;
        if (wasConnected && !this._disconnecting) this.emit("close", undefined as never);
      };
    });
  }

  /** Send raw bytes over the WebSocket. */
  send(data: Uint8Array): void {
    if (!this._connected || !this.ws) throw new Error("Not connected");
    this.ws.send(data);
  }

  /** Close the WebSocket. Idempotent. */
  async disconnect(): Promise<void> {
    if (this._disconnecting) return;
    if (!this.ws) return;

    this._disconnecting = true;
    this._connected = false;
    this.ws.onopen = null;
    this.ws.onmessage = null;
    this.ws.onerror = null;
    this.ws.onclose = null;

    if (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING) {
      this.ws.close();
    }
    this.ws = null;
    this._disconnecting = false;
    this.emit("close", undefined as never);
  }

  on<K extends keyof TransportEventMap>(event: K, handler: (data: TransportEventMap[K]) => void): void {
    if (!this.listeners.has(event)) this.listeners.set(event, new Set());
    this.listeners.get(event)!.add(handler as (data: never) => void);
  }

  off<K extends keyof TransportEventMap>(event: K, handler: (data: TransportEventMap[K]) => void): void {
    this.listeners.get(event)?.delete(handler as (data: never) => void);
  }

  private emit<K extends keyof TransportEventMap>(event: K, data: TransportEventMap[K]): void {
    const handlers = this.listeners.get(event);
    if (!handlers) return;
    for (const handler of handlers) {
      try {
        (handler as (data: TransportEventMap[K]) => void)(data);
      } catch {
        // Don't let a listener error crash the transport.
      }
    }
  }
}

/** Build the agent's MAVLink/MSP proxy URL for the current host. The proxy binds
 *  :8765 on the same host that serves this dashboard, so the browser's own host
 *  is the verified reach (wss when the page is https). */
export function mavlinkWsUrl(): string {
  const proto = window.location.protocol === "https:" ? "wss:" : "ws:";
  return `${proto}//${window.location.hostname}:8765/`;
}
