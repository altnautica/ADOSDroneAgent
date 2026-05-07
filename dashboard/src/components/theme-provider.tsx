import { useEffect } from "react";

import { useUiStore } from "@/stores/ui-store";

// Applies the persisted theme preference to the document root. The
// `system` choice follows the OS color-scheme media query and updates
// live if the OS preference flips. The Tailwind config keys off the
// `dark` class on <html>.
export function ThemeProvider({ children }: { children: React.ReactNode }) {
  const theme = useUiStore((s) => s.theme);

  useEffect(() => {
    const root = document.documentElement;

    const apply = () => {
      let resolved: "dark" | "light";
      if (theme === "system") {
        resolved = window.matchMedia("(prefers-color-scheme: dark)").matches
          ? "dark"
          : "light";
      } else {
        resolved = theme;
      }
      root.classList.toggle("dark", resolved === "dark");
      root.classList.toggle("light", resolved === "light");
    };

    apply();

    if (theme === "system") {
      const mql = window.matchMedia("(prefers-color-scheme: dark)");
      mql.addEventListener("change", apply);
      return () => mql.removeEventListener("change", apply);
    }
  }, [theme]);

  return children;
}
