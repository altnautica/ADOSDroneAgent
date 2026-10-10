// The physical-button input path. Mints a scoped WS ticket, opens the agent's
// `/ws/buttons` fanout, parses each `{button, kind, action, timestamp_ms}`
// frame (forwarded verbatim from the native `ados-pic` reader), maps it to a
// folded NavCommand, and drives the navigator. The cockpit owns the button →
// command mapping: the agent emits raw identity + phase, the
// panel decides menu semantics, so a binding change never needs an agent
// change.

import { useEffect, useState } from "react";

import { useConfirmStore } from "@/stores/confirm-store";
import { useNavStore } from "@/stores/nav-store";
import { useProfile } from "@/shared/use-profile";
import type { NavCommand } from "@/nav/navigator";
import type { ButtonEvent } from "@/lib/types";
import { WS_TICKET_PROTOCOL, mintWsTicket } from "@/shared/ws-ticket";

/** The scope a `/ws/buttons` ticket must be minted for (matches the native
 *  `SCOPE_BUTTON_EVENTS` in crates/ados-control). */
const BUTTON_SCOPE = "gs.button_events";

/** Redial backoff for a dropped button stream: 1 s, doubling to a 10 s cap,
 *  reset once a connection opens. */
export const RECONNECT_MIN_MS = 1000;
export const RECONNECT_MAX_MS = 10_000;

export function nextReconnectDelay(previousMs: number | null): number {
  return previousMs === null ? RECONNECT_MIN_MS : Math.min(RECONNECT_MAX_MS, previousMs * 2);
}

/** Default binding from a raw button identity to a folded command. The panel
 *  owns this table; on-rig the exact identity strings the `ados-pic` reader
 *  emits are confirmed against the live stream and adjusted here. Common
 *  spellings are pre-mapped so navigation works out of the box. */
const BUTTON_BINDINGS: Record<string, NavCommand> = {
  b1: "prev",
  b2: "next",
  b3: "activate",
  b4: "back",
  "1": "prev",
  "2": "next",
  "3": "activate",
  "4": "back",
  up: "prev",
  down: "next",
  prev: "prev",
  next: "next",
  select: "activate",
  enter: "activate",
  ok: "activate",
  back: "back",
  menu: "quick-menu",
  cycle: "cycle-tab",
  cycle_screen: "cycle-tab",
};

/** Resolve a button event to a command, or null when it is not actionable. A
 *  long-press of the back/menu button opens the quick menu.
 *
 *  Act on any event that is not an explicit `cancel`: the emitter sends one
 *  classified event per gesture (short/long in `kind`), and `action` is the
 *  mapped SEMANTIC (e.g. `cycle_screen`), never `press`, so gating on `action
 *  === "press"` discarded every real press. The binding keys on the stable
 *  `label` (b1..b4) the emitter now sends, falling back to the raw pin. */
function eventToCommand(ev: ButtonEvent): NavCommand | null {
  if (ev.kind === "cancel") return null;
  const id = (ev.label ?? ev.button ?? "")
    .toString()
    .trim()
    .toLowerCase();
  if (!id) return null;
  const base = BUTTON_BINDINGS[id];
  if (base == null) return null;
  if (ev.kind === "long" && (base === "back" || base === "activate")) {
    return "quick-menu";
  }
  return base;
}

export interface ButtonsState {
  connected: boolean;
}

/** Connect the button stream and drive the navigator for the app's lifetime. */
export function useButtons(): ButtonsState {
  const [connected, setConnected] = useState(false);
  const command = useNavStore((s) => s.command);
  // Physical buttons exist only on a ground station; no other profile serves
  // the stream, so nothing is dialled there.
  const isGround = useProfile() === "ground_station";

  useEffect(() => {
    if (!isGround) return;
    let closed = false;
    let delay: number | null = null;
    let socket: WebSocket | null = null;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    const controller = new AbortController();

    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${location.host}/api/v1/ground-station/ws/buttons`;

    const scheduleReconnect = () => {
      // The `reconnectTimer` guard matters as much as the `closed` one: without
      // it a churning socket overwrites a pending handle, and the overwritten
      // timer is unreachable from the cleanup below, so it survives unmount and
      // fires against a torn-down effect. The sibling reconnect loop in
      // `lib/vision-detections-ws.ts` has always had this shape; this one did
      // not.
      if (closed || reconnectTimer) return;
      delay = nextReconnectDelay(delay);
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        void connect();
      }, delay);
    };

    const connect = async () => {
      if (closed) return;
      const ticket = await mintWsTicket(BUTTON_SCOPE, controller.signal);
      if (closed) return;

      // The constructor throws synchronously on a subprotocol value that is not
      // a valid token, so a malformed ticket would otherwise become an
      // unhandled rejection and kill physical-button input for the session with
      // no reconnect. Falling back to the unticketed URL keeps the retry ladder
      // alive; the server still decides whether to accept it.
      try {
        socket = ticket
          ? new WebSocket(url, [WS_TICKET_PROTOCOL, ticket])
          : new WebSocket(url);
      } catch {
        socket = null;
        setConnected(false);
        scheduleReconnect();
        return;
      }

      socket.onopen = () => {
        setConnected(true);
        delay = null;
      };

      socket.onmessage = (msg) => {
        let frame: unknown;
        try {
          frame = JSON.parse(typeof msg.data === "string" ? msg.data : "");
        } catch {
          return;
        }
        if (!frame || typeof frame !== "object") return;
        // Skip the bus-unavailable / error frame the relay may emit.
        if ("event" in frame && (frame as { event: unknown }).event === "error") {
          return;
        }
        const cmd = eventToCommand(frame as ButtonEvent);
        const ev = frame as ButtonEvent;
        const confirm = useConfirmStore.getState();
        if (confirm.pending) {
          // A skill sheet is open: a long-press of select completes its hold,
          // back cancels it, nothing navigates underneath it.
          if (cmd === "quick-menu" && ev.kind === "long") confirm.panelConfirm();
          else if (cmd === "back") confirm.cancel();
          return;
        }
        if (cmd) command(cmd);
      };

      socket.onclose = () => {
        setConnected(false);
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
      }
    };
  }, [command, isGround]);

  return { connected };
}
