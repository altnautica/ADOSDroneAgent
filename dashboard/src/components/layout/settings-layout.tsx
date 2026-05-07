import {
  Cloud,
  Cpu,
  Monitor,
  ShieldAlert,
  UserCog,
  Wifi,
  type LucideIcon,
} from "lucide-react";
import { NavLink, Outlet } from "react-router-dom";

import { cn } from "@/lib/utils";

interface SectionLink {
  to: string;
  label: string;
  icon: LucideIcon;
  blurb: string;
}

const SECTIONS: SectionLink[] = [
  {
    to: "/settings/profile",
    label: "Profile",
    icon: UserCog,
    blurb: "Drone or ground station, role, restart on apply.",
  },
  {
    to: "/settings/network",
    label: "Network",
    icon: Wifi,
    blurb: "Wi-Fi client, hotspot toggle.",
  },
  {
    to: "/settings/cloud",
    label: "Cloud",
    icon: Cloud,
    blurb: "Altnautica relay, self-hosted, or local-only.",
  },
  {
    to: "/settings/display",
    label: "Display",
    icon: Monitor,
    blurb: "Local kiosk display selection.",
  },
  {
    to: "/settings/advanced",
    label: "Advanced",
    icon: Cpu,
    blurb: "Log level, board override, factory reset.",
  },
];

export function SettingsLayout() {
  return (
    <div className="container mx-auto px-4 py-6 max-w-5xl">
      <header className="mb-6 flex items-start gap-3">
        <ShieldAlert className="h-6 w-6 text-amber-500 mt-0.5 shrink-0" />
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Settings</h1>
          <p className="text-sm text-muted-foreground mt-1">
            Live agent configuration. Risky changes (passwords, profile switches,
            factory reset) require explicit Save and a confirm prompt.
          </p>
        </div>
      </header>

      <div className="grid grid-cols-1 md:grid-cols-[220px_1fr] gap-6">
        <nav className="space-y-1">
          {SECTIONS.map((s) => {
            const Icon = s.icon;
            return (
              <NavLink
                key={s.to}
                to={s.to}
                className={({ isActive }) =>
                  cn(
                    "flex items-start gap-3 rounded-md px-3 py-2 text-sm transition-colors",
                    isActive
                      ? "bg-accent text-accent-foreground"
                      : "text-muted-foreground hover:bg-accent/50 hover:text-foreground",
                  )
                }
              >
                <Icon className="h-4 w-4 mt-0.5 shrink-0" />
                <div className="flex-1 min-w-0">
                  <div className="font-medium">{s.label}</div>
                  <div className="text-[11px] text-muted-foreground/80 mt-0.5">
                    {s.blurb}
                  </div>
                </div>
              </NavLink>
            );
          })}
        </nav>

        <main className="min-w-0">
          <Outlet />
        </main>
      </div>
    </div>
  );
}
