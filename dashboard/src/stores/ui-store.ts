import { create } from "zustand";
import { persist } from "zustand/middleware";

import type { RestartKind, RestartPending } from "@/lib/restart-pending";

export type Theme = "dark" | "light" | "system";

interface UiState {
  theme: Theme;
  // sidebarCollapsed governs the *desktop* sidebar (lg+), where the
  // sidebar is always visible and only its width changes.
  sidebarCollapsed: boolean;
  // mobileNavOpen governs the *mobile* drawer (below lg), where the
  // sidebar is hidden by default and slides in from the left when
  // toggled. The hamburger in the header toggles this.
  mobileNavOpen: boolean;
  // A restart the operator asked for (reboot, profile apply); never persisted.
  restartPending: RestartPending | null;
  beginRestart: (kind: RestartKind, uptimeS: number | null) => void;
  markRestartDown: () => void;
  clearRestart: () => void;
  setTheme: (t: Theme) => void;
  toggleSidebar: () => void;
  toggleMobileNav: () => void;
  closeMobileNav: () => void;
}

export const useUiStore = create<UiState>()(
  persist(
    (set) => ({
      theme: "system",
      sidebarCollapsed: false,
      mobileNavOpen: false,
      restartPending: null,
      beginRestart: (kind, uptimeS) =>
        set({ restartPending: { kind, since: Date.now(), baselineUptimeS: uptimeS, sawDown: false } }),
      markRestartDown: () =>
        set((s) =>
          s.restartPending && !s.restartPending.sawDown
            ? { restartPending: { ...s.restartPending, sawDown: true } }
            : s,
        ),
      clearRestart: () => set({ restartPending: null }),
      setTheme: (theme) => set({ theme }),
      toggleSidebar: () =>
        set((s) => ({ sidebarCollapsed: !s.sidebarCollapsed })),
      toggleMobileNav: () => set((s) => ({ mobileNavOpen: !s.mobileNavOpen })),
      closeMobileNav: () => set({ mobileNavOpen: false }),
    }),
    {
      name: "ados.ui",
      // mobileNavOpen is transient — never persist a "stuck open" drawer.
      partialize: (s) => ({
        theme: s.theme,
        sidebarCollapsed: s.sidebarCollapsed,
      }),
    },
  ),
);
