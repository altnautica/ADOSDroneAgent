import { Home, Settings as SettingsIcon } from "lucide-react";
import { NavLink } from "react-router-dom";

import { cn } from "@/lib/utils";

const SLOTS = [
  { to: "/", label: "Home", icon: Home },
  { to: "/settings", label: "Settings", icon: SettingsIcon },
];

export function BottomDock() {
  return (
    <nav
      className="lg:hidden border-t border-border bg-background flex items-center justify-around h-14 sticky bottom-0 z-30"
      aria-label="primary"
    >
      {SLOTS.map(({ to, label, icon: Icon }) => (
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
          <span className="text-[10px]">{label}</span>
        </NavLink>
      ))}
    </nav>
  );
}
