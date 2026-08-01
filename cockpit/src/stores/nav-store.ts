// The navigator state machine — active tab, detail/modal stack, the
// button/gamepad focus ring, and the quick menu. Mirrors the TFT
// `PageNavigator`: one place decides what is on screen. The active tab is
// persisted to localStorage so a kiosk reload returns to the same screen
// (the TFT persists to /run/ados/lcd-state.json for the same reason).

import { create } from "zustand";

import {
  DEFAULT_TAB_ID,
  TAB_ORDER,
  type NavCommand,
  type ScreenAction,
} from "@/nav/navigator";

const PERSIST_KEY = "ados-cockpit-active-tab";

function loadPersistedTab(): string {
  if (typeof localStorage === "undefined") return DEFAULT_TAB_ID;
  try {
    const saved = localStorage.getItem(PERSIST_KEY);
    if (saved && TAB_ORDER.includes(saved)) return saved;
  } catch {
    // storage disabled — fall through to the default
  }
  return DEFAULT_TAB_ID;
}

function persistTab(id: string): void {
  if (typeof localStorage === "undefined") return;
  try {
    localStorage.setItem(PERSIST_KEY, id);
  } catch {
    // no-op
  }
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
  /** The focus-ring index across the tab menu (moved by buttons/gamepad). */
  menuFocusIndex: number;
  /** Whether the quick menu overlay is open. */
  quickMenuOpen: boolean;
  /** Whether the menu chrome is collapsed to give the feed the whole panel. */
  menuCollapsed: boolean;
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
  /** Declare which tabs this profile shows. Idempotent; the shell calls it once
   *  the profile probe resolves. */
  setVisibleTabs: (ids: string[]) => void;
  toggleMenuCollapsed: () => void;
  closeQuickMenu: () => void;
}

export const useNavStore = create<NavState>((set, get) => ({
  activeTabId: loadPersistedTab(),
  detailStack: [],
  menuFocusIndex: tabIndex(loadPersistedTab()),
  quickMenuOpen: false,
  menuCollapsed: false,
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
        // In the quick menu, activate the focused tab; on a screen it acts as
        // "open the focused menu entry".
        {
          const target = s.visibleTabs[s.menuFocusIndex];
          if (target) s.goTab(target);
        }
        break;
      case "back":
        if (s.quickMenuOpen) {
          set({ quickMenuOpen: false });
        } else {
          s.back();
        }
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
    persistTab(id);
    set({
      activeTabId: id,
      detailStack: [],
      menuFocusIndex: tabIndex(id),
      quickMenuOpen: false,
    });
  },

  openDetail: (id) => {
    set((s) => ({ detailStack: [...s.detailStack, id], quickMenuOpen: false }));
  },

  back: () => {
    set((s) =>
      s.detailStack.length > 0
        ? { detailStack: s.detailStack.slice(0, -1) }
        : {},
    );
  },

  setMenuFocus: (index) => {
    // Bounded by the VISIBLE set, because that is what the index means: the
    // menu renders the visible tabs, so a caller handing over a rendered
    // position is speaking in those terms.
    if (index < 0 || index >= get().visibleTabs.length) return;
    set({ menuFocusIndex: index });
  },

  setVisibleTabs: (ids) =>
    set((s) => {
      if (
        s.visibleTabs.length === ids.length &&
        s.visibleTabs.every((id, i) => id === ids[i])
      ) {
        return s;
      }
      // Keep the focus ring on the tab it was on if that tab survived, so a
      // profile resolving mid-session does not silently move the operator's
      // selection; otherwise fall back to the active tab, then to the start.
      const focused = s.visibleTabs[s.menuFocusIndex];
      const kept = focused ? ids.indexOf(focused) : -1;
      const onActive = ids.indexOf(s.activeTabId);
      const menuFocusIndex = kept >= 0 ? kept : onActive >= 0 ? onActive : 0;
      return { ...s, visibleTabs: ids, menuFocusIndex };
    }),

  toggleMenuCollapsed: () => set((s) => ({ menuCollapsed: !s.menuCollapsed })),

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
