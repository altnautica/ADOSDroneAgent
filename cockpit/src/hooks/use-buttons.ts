// The physical-button input path. Mints a scoped WS ticket, opens the agent's
// `/ws/buttons` fanout, parses each `{button, kind, action, timestamp_ms}`
// frame (forwarded verbatim from the native `ados-pic` reader), maps it to a
// folded NavCommand, and drives the navigator. The cockpit owns the button →
// command mapping: the agent emits raw identity + phase, the
// panel decides menu semantics, so a binding change never needs an agent
// change.

import { useEffect, useState } from "react";

import { completePanelHold } from "@/lib/hold-input";
import { executeSkill, requestSkill } from "@/lib/skill-runner";
import { skillContextNow } from "@/hooks/use-skill-context";
import { BUILTIN_SKILL_BY_ID } from "@/lib/skills";
import { ReconnectBackoff } from "@/lib/reconnect-backoff";
import { useConfirmStore } from "@/stores/confirm-store";
import { useInputStore } from "@/stores/input-store";
import { useNavStore } from "@/stores/nav-store";
import { useProfile } from "@/shared/use-profile";
import type { NavCommand } from "@/nav/navigator";
import type { ButtonEvent } from "@/lib/types";
import { WS_TICKET_PROTOCOL, mintWsTicket } from "@/shared/ws-ticket";

/** The scope a `/ws/buttons` ticket must be minted for (matches the native
 *  `SCOPE_BUTTON_EVENTS` in crates/ados-control). */
const BUTTON_SCOPE = "gs.button_events";

/** Redial backoff for a dropped button stream: 1 s, doubling to a 10 s cap,
 *  reset only after a session that delivered an event or stayed open. */
const RECONNECT_MIN_MS = 1000;
const RECONNECT_MAX_MS = 10_000;

/** The panel action that hands a button to plugins (see the agent's
 *  `panel_ui::PASSTHROUGH`). */
const PASSTHROUGH_ACTION = "passthrough";

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
  // The rotary encoder: a detent steps the focus, the push switch fires it.
  enc_cw: "next",
  enc_ccw: "prev",
  enc_sw: "activate",
};

/** The encoder push switch's label; its long press goes back (the agent's
 *  default `ENC_SW_long` is `back`), not to the quick menu. */
const ENCODER_SWITCH = "enc_sw";

/** Resolve a button event to a command, or null when it is not actionable. A
 *  long-press of the back/menu button opens the quick menu.
 *
 *  Act on any event that is not an explicit `cancel`: the emitter sends one
 *  classified event per gesture (short/long in `kind`), and `action` is the
 *  mapped SEMANTIC (e.g. `cycle_screen`), never `press`, so gating on `action
 *  === "press"` discarded every real press. The binding keys on the stable
 *  `label` (b1..b4) the emitter now sends, falling back to the raw pin.
 *
 *  An event whose `action` is `passthrough` is reserved for plugins and never
 *  drives the panel navigator. */
export function eventToCommand(ev: ButtonEvent): NavCommand | null {
  if (ev.kind === "cancel") return null;
  if (ev.action === PASSTHROUGH_ACTION) return null;
  const id = (ev.label ?? ev.button ?? "")
    .toString()
    .trim()
    .toLowerCase();
  if (!id) return null;
  const base = BUTTON_BINDINGS[id];
  if (base == null) return null;
  if (ev.kind === "long" && id === ENCODER_SWITCH) return "back";
  if (ev.kind === "long" && (base === "back" || base === "activate")) {
    return "quick-menu";
  }
  return base;
}

/** Whether the event is a long press of the panel's select button (a button
 *  whose base command is `activate`, never the encoder switch, whose long
 *  press goes back). Only this counts toward the panel's two-step STOP; a
 *  long press of back keeps its own meaning (the menu). */
export function isLongSelect(ev: ButtonEvent): boolean {
  if (ev.kind !== "long" || ev.action === PASSTHROUGH_ACTION) return false;
  const id = (ev.label ?? ev.button ?? "").toString().trim().toLowerCase();
  return id !== ENCODER_SWITCH && BUTTON_BINDINGS[id] === "activate";
}

/** How long the ground station holds a button before it reports a long press
 *  (the panel's `LONG_PRESS_SECONDS`, 2 s). */
export const PANEL_LONG_PRESS_MS = 2000;

/** A panel long-press is a completed hold in one gesture only for a hold no
 *  longer than the long press itself: it never completes the 3 s emergency
 *  stop, which the panel cannot time. */
export function panelLongPressCompletes(confirmMs: number): boolean {
  return confirmMs <= PANEL_LONG_PRESS_MS;
}

/** How long the STOP sheet must have been open before a panel long-press
 *  fires the emergency stop. */
export const PANEL_STOP_ARM_MS = 1000;

/** The panel's emergency stop. The panel reports a long press only on
 *  release, so it cannot time the 3 s hold; it takes two deliberate long
 *  presses instead. A long press of select on the focused STOP opens the
 *  STOP sheet and arms it; a second long press, `PANEL_STOP_ARM_MS` after the
 *  arming one, fires the stop. A sheet opened any other way is armed by its
 *  first long select. One long press never fires it. */
export interface PanelDeps {
  command: (cmd: NavCommand) => void;
  /** Whether the focused control is STOP. */
  stopFocused: () => boolean;
  now: () => number;
}

/** Act on one classified panel event. */
export function dispatchPanelEvent(ev: ButtonEvent, deps: PanelDeps): void {
  const cmd = eventToCommand(ev);
  if (cmd) useInputStore.getState().setSource("panel");
  const longSelect = isLongSelect(ev);
  const confirm = useConfirmStore.getState();
  if (confirm.pending) {
    // A skill sheet is open: back cancels it, nothing navigates underneath
    // it. A long press of select fires the STOP once its sheet has been open
    // a moment, and completes any other hold no longer than the long press.
    const { skill, panelArmedAt, altitudeM } = confirm.pending;
    if (longSelect && skill.id === BUILTIN_SKILL_BY_ID.kill.id) {
      // Only a panel long select arms the stop: the first one on a sheet
      // opened any other way arms it, and a later one fires it.
      if (panelArmedAt === null) confirm.armPanel(deps.now());
      else if (deps.now() - panelArmedAt >= PANEL_STOP_ARM_MS) {
        confirm.cancel();
        void executeSkill(skill, { altitudeM });
      }
    } else if (longSelect && panelLongPressCompletes(skill.confirm.ms)) confirm.panelConfirm();
    else if (cmd === "back" || (ev.kind === "long" && cmd === "quick-menu")) confirm.cancel();
    return;
  }
  // A long press of select on the focused STOP opens its sheet through the
  // same gate as the STOP button's click; it never fires it.
  if (longSelect && deps.stopFocused()) {
    if (requestSkill(BUILTIN_SKILL_BY_ID.kill, skillContextNow())) useConfirmStore.getState().armPanel(deps.now());
    return;
  }
  // A focused guarded button or slide-to-confirm takes the long press as
  // its completed hold.
  if (longSelect && completePanelHold()) return;
  if (cmd) deps.command(cmd);
}

/** Whether the document's focused control is STOP. */
function focusedIsStop(): boolean {
  const el = typeof document === "undefined" ? null : document.activeElement;
  return el instanceof HTMLElement && el.closest('[data-zone="stop"]') !== null;
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
    const backoff = new ReconnectBackoff(RECONNECT_MIN_MS, RECONNECT_MAX_MS);
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
      reconnectTimer = setTimeout(() => {
        reconnectTimer = null;
        void connect();
      }, backoff.next());
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
        backoff.opened();
      };

      socket.onmessage = (msg) => {
        backoff.message();
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
        dispatchPanelEvent(frame as ButtonEvent, {
          command,
          stopFocused: focusedIsStop,
          now: () => performance.now(),
        });
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
