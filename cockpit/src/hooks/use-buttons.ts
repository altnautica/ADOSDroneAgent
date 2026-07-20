// The physical-button input path. Mints a scoped WS ticket, opens the agent's
// `/ws/buttons` fanout, parses each `{button, kind, action, timestamp_ms}`
// frame (forwarded verbatim from the native `ados-pic` reader), maps it to a
// folded NavCommand, and drives the navigator. The cockpit owns the button →
// command mapping: the agent emits raw identity + phase, the
// panel decides menu semantics, so a binding change never needs an agent
// change.

import { useEffect, useState } from "react";

import { useNavStore } from "@/stores/nav-store";
import type { NavCommand } from "@/nav/navigator";
import type { ButtonEvent } from "@/lib/types";
import { WS_TICKET_PROTOCOL, mintWsTicket } from "@/lib/ws-ticket";

/** The scope a `/ws/buttons` ticket must be minted for (matches the native
 *  `SCOPE_BUTTON_EVENTS` in crates/ados-control). */
const BUTTON_SCOPE = "gs.button_events";

const RECONNECT_MIN_MS = 1000;
const RECONNECT_MAX_MS = 10_000;

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

/** Act only on the leading edge of a gesture so a press+release pair does not
 *  fire twice. If the frame carries a press/hold/release `action`, act on
 *  `press`; otherwise the frame is one classified event per gesture, so act on
 *  anything that is not an explicit `cancel`. */
function shouldAct(ev: ButtonEvent): boolean {
  if (ev.action) return ev.action === "press";
  return ev.kind !== "cancel";
}

/** Resolve a button event to a command, or null when it is not actionable. A
 *  long-press of the back/menu button opens the quick menu. */
function eventToCommand(ev: ButtonEvent): NavCommand | null {
  if (!shouldAct(ev)) return null;
  const id = (ev.button ?? "").toString().trim().toLowerCase();
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

  useEffect(() => {
    let closed = false;
    let socket: WebSocket | null = null;
    let reconnectMs = RECONNECT_MIN_MS;
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
    const controller = new AbortController();

    const proto = location.protocol === "https:" ? "wss:" : "ws:";
    const url = `${proto}//${location.host}/api/v1/ground-station/ws/buttons`;

    const scheduleReconnect = () => {
      if (closed) return;
      reconnectTimer = setTimeout(connect, reconnectMs);
      reconnectMs = Math.min(reconnectMs * 2, RECONNECT_MAX_MS);
    };

    const connect = async () => {
      if (closed) return;
      const ticket = await mintWsTicket(BUTTON_SCOPE, controller.signal);
      if (closed) return;

      socket = ticket
        ? new WebSocket(url, [WS_TICKET_PROTOCOL, ticket])
        : new WebSocket(url);

      socket.onopen = () => {
        reconnectMs = RECONNECT_MIN_MS;
        setConnected(true);
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
  }, [command]);

  return { connected };
}
