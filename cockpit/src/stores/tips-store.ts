// Contextual tips on this device, persisted to localStorage `ados-cockpit-tips`
// (cockpit spec `tips.json` `persist: per_device`): the tips dismissed for
// good and how often the flight screen has opened. The kiosk keeps it in its
// persistent browser profile. The queue is the shared engine
// (`@/shared/hud/tips`); this store remembers, and runs one flight-screen
// open's session: when it opened, the tip on screen, the last dismissal and
// the tips auto-dismissed unseen on this open (a tip hidden by a banner, a
// sheet or the tour goes back to the queue).

import { create } from "zustand";

import {
  dismissTip,
  hiddenThisOpenAfter,
  nextTip,
  onFlightScreenOpen,
  resetTips,
  type TipDecision,
  type TipsContext,
  type TipsState,
} from "@/shared/hud/tips";

export const TIPS_KEY = "ados-cockpit-tips";

function load(): TipsState {
  const base = resetTips();
  if (typeof localStorage === "undefined") return base;
  try {
    const raw = localStorage.getItem(TIPS_KEY);
    if (!raw) return base;
    const p = JSON.parse(raw) as { seen?: unknown; open_count?: unknown };
    return {
      seen: Array.isArray(p.seen) ? p.seen.filter((s): s is string => typeof s === "string") : [],
      openCount: typeof p.open_count === "number" && Number.isFinite(p.open_count) && p.open_count > 0 ? Math.floor(p.open_count) : 0,
    };
  } catch {
    return base;
  }
}

function persist(s: TipsState): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(TIPS_KEY, JSON.stringify({ seen: s.seen, open_count: s.openCount }));
  } catch {
    // storage blocked or full: the change lasts this session
  }
}

/** One flight-screen open. */
export interface TipSession {
  openedAtMs: number;
  lastDismissedAtMs: number | null;
  shownTipId: string | null;
  shownAtMs: number | null;
  hiddenThisOpen: string[];
}

/** What the flight screen reports each tick: everything in `TipsContext`
 *  but the session's own clock. */
export type TipsScreen = Omit<TipsContext, "screenOpenedAtMs" | "lastDismissedAtMs" | "shownTipId" | "shownAtMs" | "hiddenThisOpen">;

interface TipsStore {
  tips: TipsState;
  session: TipSession | null;
  /** The flight screen mounted: counts one open and starts its session. */
  flightScreenOpened: (nowMs: number) => void;
  /** The flight screen unmounted. */
  flightScreenClosed: () => void;
  /** Re-decide with what is on screen now; returns the decision applied. */
  tick: (screen: TipsScreen, nowMs: number) => TipDecision;
  /** The pilot dismissed the tip on screen (tapped it, its anchor, its
   *  button, or pressed a controller button): it is seen for good. */
  dismissShown: (nowMs: number) => void;
  /** Settings › Reset tips: every tip shows again. */
  reset: () => void;
  /** Re-read storage (tests). */
  reload: () => void;
}

export const useTipsStore = create<TipsStore>((set, get) => {
  const commit = (tips: TipsState) => {
    set({ tips });
    persist(tips);
  };
  const hide = (s: TipSession, nowMs: number, hiddenUnseen: string | null): TipSession => ({
    ...s,
    shownTipId: null,
    shownAtMs: null,
    lastDismissedAtMs: nowMs,
    hiddenThisOpen: hiddenUnseen ? [...s.hiddenThisOpen, hiddenUnseen] : s.hiddenThisOpen,
  });
  return {
    tips: load(),
    session: null,
    flightScreenOpened: (nowMs) => {
      commit(onFlightScreenOpen(get().tips));
      set({ session: { openedAtMs: nowMs, lastDismissedAtMs: null, shownTipId: null, shownAtMs: null, hiddenThisOpen: [] } });
    },
    flightScreenClosed: () => set({ session: null }),
    tick: (screen, nowMs) => {
      const s = get().session;
      if (!s) return { kind: "none" };
      const d = nextTip(
        get().tips,
        {
          ...screen,
          screenOpenedAtMs: s.openedAtMs,
          lastDismissedAtMs: s.lastDismissedAtMs,
          shownTipId: s.shownTipId,
          shownAtMs: s.shownAtMs,
          hiddenThisOpen: s.hiddenThisOpen,
        },
        nowMs,
      );
      if (d.kind === "show" && s.shownTipId !== d.tipId) set({ session: { ...s, shownTipId: d.tipId, shownAtMs: nowMs } });
      else if (d.kind === "hide") {
        if (d.markSeen) commit(dismissTip(get().tips, d.tipId));
        set({ session: hide(s, nowMs, hiddenThisOpenAfter(d) ? d.tipId : null) });
      }
      return d;
    },
    dismissShown: (nowMs) => {
      const s = get().session;
      if (!s?.shownTipId) return;
      commit(dismissTip(get().tips, s.shownTipId));
      set({ session: hide(s, nowMs, null) });
    },
    reset: () => commit(resetTips()),
    reload: () => set({ tips: load() }),
  };
});

/** The Feed's flow overlays (launch step and Abort, handover, link-loss and
 *  low-battery countdowns, sticks with another surface). They sit outside the
 *  alert stack, so tips name them on their own. */
export const FLIGHT_OVERLAYS_SELECTOR = "[data-testid=flight-overlays]";

/** The flight screen's banners: each shell notice (agent offline, reach,
 *  reboot), each alert in the alert stack, and each flow overlay (the sticks
 *  banner carries no role, so every overlay child counts). */
export const ALERT_BANNER_SELECTOR = [
  '[data-testid="feed-notices"] > *',
  "[data-zone=alert_stack] [role=alert] > *",
  "[data-zone=alert_stack] [role=status] > *",
  `${FLIGHT_OVERLAYS_SELECTOR} [role=alert]`,
  `${FLIGHT_OVERLAYS_SELECTOR} [role=status]`,
  `${FLIGHT_OVERLAYS_SELECTOR} > *`,
].join(", ");

/** Whether any flight-screen banner is on screen (tips `show_only_when`
 *  `alert_banner_visible: false`); `visible` says whether an element is laid
 *  out and on screen. */
export function alertBannerVisible(root: Pick<ParentNode, "querySelectorAll">, visible: (el: Element) => boolean): boolean {
  return Array.from(root.querySelectorAll(ALERT_BANNER_SELECTOR)).some(visible);
}
