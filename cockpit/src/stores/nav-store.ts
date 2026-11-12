// The navigator state machine — active tab, detail/modal stack, the
// button/gamepad focus ring, and the quick menu. Mirrors the TFT
// `PageNavigator`: one place decides what is on screen. The active tab is
// persisted to localStorage so a kiosk reload returns to the same screen
// (the TFT persists to /run/ados/lcd-state.json for the same reason). With
// nothing persisted, the cockpit opens on the profile's home screen once the
// profile is known.

import { create } from "zustand";

import { offerCommand } from "@/lib/command-stack";
import { knownOnGround } from "@/lib/controller-tour";
import {
  DEFAULT_TAB_ID,
  type NavCommand,
  type ScreenAction,
} from "@/nav/navigator";
import { SCREEN_TABLE, TAB_ORDER } from "@/nav/screen-table";
import { pilotFresh } from "@/shared/hud/pilot-fresh";
import { useDirtyStore } from "@/stores/dirty-store";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

const PERSIST_KEY = "ados-cockpit-active-tab";

/** The tab the operator last chose, or null when none was saved. */
function loadPersistedTab(): string | null {
  if (typeof localStorage === "undefined") return null;
  try {
    const saved = localStorage.getItem(PERSIST_KEY);
    if (saved && TAB_ORDER.includes(saved)) return saved;
  } catch {
    // storage disabled — fall through to the default
  }
  return null;
}

const PERSISTED_TAB = loadPersistedTab();

function persistTab(id: string): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(PERSIST_KEY, id);
  } catch {
    // no-op
  }
}

/** Whether `id` is the flight screen (the full-bleed Feed). */
function isFlightScreen(id: string): boolean {
  return SCREEN_TABLE.some((m) => m.id === id && m.fullBleed === true);
}

/** Leave for the flight screen through the dirty guard, unless the vehicle
 *  may be flying (anything but known disarmed on the ground, an unknown arm
 *  state included): then the pilot gets the Feed at once, the drafts are left
 *  as they are, and a leave already parked behind the dialog is dropped so
 *  its answer cannot pull the pilot back off the Feed. */
function leaveForFlight(leave: () => void): void {
  const pilot = useNavStreamStore.getState().pilot;
  const flight = useFlightStore.getState();
  const fresh = pilot !== null && pilotFresh(pilot.at, Date.now()) ? pilot.data : null;
  if (knownOnGround(fresh, { live: flight.live && !flight.stale, armed: flight.telemetry?.armed ?? null })) {
    useDirtyStore.getState().requestLeave(leave);
    return;
  }
  useDirtyStore.getState().keepEditing();
  leave();
}

function tabIndex(id: string): number {
  const i = TAB_ORDER.indexOf(id);
  return i < 0 ? 0 : i;
}

interface NavState {
  /** The active top-level tab id. */
  activeTabId: string;
  /** Detail screens stacked above the active tab, top of stack last. */
  detailStack: string[];
  /** Whether the operator chose the active tab (it was persisted or picked
   *  this session). An unchosen tab follows the profile's home screen. */
  tabChosen: boolean;
  /** The focus-ring index across the tab menu (moved by buttons/gamepad). */
  menuFocusIndex: number;
  /** Whether the quick menu overlay is open. */
  quickMenuOpen: boolean;
  /** The tabs actually shown for this node's profile, in menu order.
   *
   *  Navigation walks THIS, never the full registry order. A drone hides the
   *  ground-station-only screens, and stepping the unfiltered list meant the
   *  panel's own next/activate buttons could land on a tab with no menu entry,
   *  polling routes that do not exist on the profile — a screen the operator
   *  could see but not have chosen and could not obviously leave. It also made
   *  the focus ring compare a filtered index against an unfiltered one, so on a
   *  drone the ring highlighted nothing. */
  visibleTabs: string[];

  /** Apply a screen-raised action (the analog of dispatching a TFT HitAction). */
  dispatch: (action: ScreenAction) => void;
  /** Apply a folded input command from the single dispatcher. */
  command: (cmd: NavCommand) => void;
  goTab: (id: string) => void;
  openDetail: (id: string) => void;
  back: () => void;
  setMenuFocus: (index: number) => void;
  /** Declare which tabs this profile shows and its home tab (null until the
   *  profile is known). Idempotent; the shell calls it once the profile probe
   *  resolves. An unchosen active tab, or one the profile hides, moves to the
   *  home tab. */
  setVisibleTabs: (ids: string[], home?: string | null) => void;
  closeQuickMenu: () => void;
}

export const useNavStore = create<NavState>((set, get) => ({
  activeTabId: PERSISTED_TAB ?? DEFAULT_TAB_ID,
  detailStack: [],
  tabChosen: PERSISTED_TAB !== null,
  menuFocusIndex: tabIndex(PERSISTED_TAB ?? DEFAULT_TAB_ID),
  quickMenuOpen: false,
  // Until the profile resolves, every tab is assumed visible — the historical
  // shape, and the safe direction: a tab that turns out to be hidden is removed
  // on the next update rather than being unreachable in the meantime.
  visibleTabs: [...TAB_ORDER],

  dispatch: (action) => {
    switch (action.kind) {
      case "go-tab":
        get().goTab(action.id);
        break;
      case "open-detail":
        get().openDetail(action.id);
        break;
      case "back":
        get().back();
        break;
      case "custom":
        // Screen-defined keys are handled by the owning screen, not the
        // navigator. Nothing to route here.
        break;
    }
  },

  command: (cmd) => {
    // An open overlay, or a screen that owns the command, takes it first.
    if (offerCommand(cmd)) return;
    const s = get();
    switch (cmd) {
      case "prev": {
        const tabs = s.visibleTabs;
        if (tabs.length === 0) break;
        const next = (s.menuFocusIndex - 1 + tabs.length) % tabs.length;
        set({ menuFocusIndex: next });
        break;
      }
      case "next": {
        const tabs = s.visibleTabs;
        if (tabs.length === 0) break;
        const next = (s.menuFocusIndex + 1) % tabs.length;
        set({ menuFocusIndex: next });
        break;
      }
      case "activate":
        // With no overlay open, activate opens the focused menu entry (an open
        // quick menu or sheet takes the command first, through the stack).
        {
          const target = s.visibleTabs[s.menuFocusIndex];
          if (target) s.goTab(target);
        }
        break;
      case "back":
        s.back();
        break;
      case "cycle-tab": {
        const tabs = s.visibleTabs;
        if (tabs.length === 0) break;
        const here = tabs.indexOf(s.activeTabId);
        // An active tab that is not in the visible set (a profile change while
        // it was open) steps to the first visible one rather than nowhere.
        const next = here < 0 ? 0 : (here + 1) % tabs.length;
        s.goTab(tabs[next]);
        break;
      }
      case "quick-menu":
        set({ quickMenuOpen: !s.quickMenuOpen });
        break;
    }
  },

  goTab: (id) => {
    if (!TAB_ORDER.includes(id)) return;
    const s0 = get();
    const go = () => {
      persistTab(id);
      set((s) => ({
        activeTabId: id,
        detailStack: [],
        tabChosen: true,
        // The ring indexes the VISIBLE tabs, the list the menu renders.
        menuFocusIndex: Math.max(0, s.visibleTabs.indexOf(id)),
        quickMenuOpen: false,
      }));
    };
    // Leaving the current screen with unsaved drafts asks first (dirty-store).
    if (s0.activeTabId === id && s0.detailStack.length === 0) go();
    else if (isFlightScreen(id)) leaveForFlight(go);
    else useDirtyStore.getState().requestLeave(go);
  },

  openDetail: (id) => {
    set((s) => ({ detailStack: [...s.detailStack, id], quickMenuOpen: false }));
  },

  back: () => {
    const { detailStack, activeTabId } = get();
    if (detailStack.length === 0) return;
    const pop = () => set((s) => (s.detailStack.length > 0 ? { detailStack: s.detailStack.slice(0, -1) } : {}));
    if (detailStack.length === 1 && isFlightScreen(activeTabId)) leaveForFlight(pop);
    else useDirtyStore.getState().requestLeave(pop);
  },

  setMenuFocus: (index) => {
    // Bounded by the VISIBLE set, because that is what the index means: the
    // menu renders the visible tabs, so a caller handing over a rendered
    // position is speaking in those terms.
    if (index < 0 || index >= get().visibleTabs.length) return;
    set({ menuFocusIndex: index });
  },

  setVisibleTabs: (ids, home = null) =>
    set((s) => {
      // The active tab follows the profile's home when the operator never
      // chose one, or when the chosen one is hidden on this profile.
      const homeVisible = home !== null && ids.includes(home);
      const relocate =
        homeVisible && s.activeTabId !== home && (!s.tabChosen || !ids.includes(s.activeTabId));
      const sameTabs =
        s.visibleTabs.length === ids.length && s.visibleTabs.every((id, i) => id === ids[i]);
      if (sameTabs && !relocate) return s;
      const activeTabId = relocate ? (home as string) : s.activeTabId;
      // Keep the focus ring on the tab it was on if that tab survived, so a
      // profile resolving mid-session does not silently move the operator's
      // selection; otherwise fall back to the active tab, then to the start.
      const focused = relocate ? undefined : s.visibleTabs[s.menuFocusIndex];
      const kept = focused ? ids.indexOf(focused) : -1;
      const onActive = ids.indexOf(activeTabId);
      const menuFocusIndex = kept >= 0 ? kept : onActive >= 0 ? onActive : 0;
      return {
        ...s,
        visibleTabs: ids,
        menuFocusIndex,
        activeTabId,
        detailStack: relocate ? [] : s.detailStack,
      };
    }),

  closeQuickMenu: () => set({ quickMenuOpen: false }),
}));

/** The id of the screen the shell should render: the top detail screen when
 *  the stack is non-empty, else the active tab. */
export function activeScreenId(state: {
  activeTabId: string;
  detailStack: string[];
}): string {
  return state.detailStack.length > 0
    ? state.detailStack[state.detailStack.length - 1]
    : state.activeTabId;
}
