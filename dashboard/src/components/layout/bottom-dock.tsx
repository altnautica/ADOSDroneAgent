import { Home, Settings as SettingsIcon } from "lucide-react";
import type { ParseKeys } from "i18next";
import { useTranslation } from "react-i18next";
import { NavLink } from "react-router-dom";

import { cn } from "@/lib/utils";

const SLOTS: { to: string; labelKey: ParseKeys; icon: typeof Home }[] = [
  { to: "/", labelKey: "nav.home", icon: Home },
  { to: "/settings", labelKey: "nav.settings", icon: SettingsIcon },
];

export function BottomDock() {
  const { t } = useTranslation();
  return (
    <nav
      className="lg:hidden border-t border-border bg-background flex items-center justify-around h-14 sticky bottom-0 z-30"
      aria-label={t("nav.primary")}
    >
      {SLOTS.map(({ to, labelKey, icon: Icon }) => (
        <NavLink
          key={to}
          to={to}
          end={to === "/"}
          className={({ isActive }) =>
            cn(
              "flex flex-col items-center justify-center gap-0.5 flex-1 h-full text-xs transition-colors",
              isActive ? "text-primary" : "text-muted-foreground hover:text-foreground",
            )
          }
        >
          <Icon className="h-4 w-4" />
          <span className="text-[10px]">{t(labelKey)}</span>
        </NavLink>
      ))}
    </nav>
  );
}
