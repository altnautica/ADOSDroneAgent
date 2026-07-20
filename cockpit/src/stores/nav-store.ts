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

  /** Apply a screen-raised action (the analog of dispatching a TFT HitAction). */
  dispatch: (action: ScreenAction) => void;
  /** Apply a folded input command from the single dispatcher. */
  command: (cmd: NavCommand) => void;
  goTab: (id: string) => void;
  openDetail: (id: string) => void;
  back: () => void;
  setMenuFocus: (index: number) => void;
  toggleMenuCollapsed: () => void;
  closeQuickMenu: () => void;
}

export const useNavStore = create<NavState>((set, get) => ({
  activeTabId: loadPersistedTab(),
  detailStack: [],
  menuFocusIndex: tabIndex(loadPersistedTab()),
  quickMenuOpen: false,
  menuCollapsed: false,

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
        const next = (s.menuFocusIndex - 1 + TAB_ORDER.length) % TAB_ORDER.length;
        set({ menuFocusIndex: next });
        break;
      }
      case "next": {
        const next = (s.menuFocusIndex + 1) % TAB_ORDER.length;
        set({ menuFocusIndex: next });
        break;
      }
      case "activate":
        // In the quick menu, activate the focused tab; on a screen it acts as
        // "open the focused menu entry".
        s.goTab(TAB_ORDER[s.menuFocusIndex]);
        break;
      case "back":
        if (s.quickMenuOpen) {
          set({ quickMenuOpen: false });
        } else {
          s.back();
        }
        break;
      case "cycle-tab": {
        const next = (tabIndex(s.activeTabId) + 1) % TAB_ORDER.length;
        s.goTab(TAB_ORDER[next]);
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
    if (index < 0 || index >= TAB_ORDER.length) return;
    set({ menuFocusIndex: index });
  },

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
