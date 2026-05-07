import { create } from "zustand";
import { persist } from "zustand/middleware";

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
