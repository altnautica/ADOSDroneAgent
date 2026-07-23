import {
  Boxes,
  Cloud,
  Cpu,
  Eye,
  Fingerprint,
  Globe,
  HeartPulse,
  Lock,
  Monitor,
  Radar,
  Route,
  Share2,
  ShieldAlert,
  Signal,
  UserCog,
  Wifi,
  type LucideIcon,
} from "lucide-react";
import { NavLink, Outlet } from "react-router-dom";

import { cn } from "@/lib/utils";

// The settings nav is grouped into two tiers: a small uppercase group header
// over its section links. The nav grew past a dozen entries once the curated
// on-box config pages landed, so a flat list stopped being scannable.
type SettingsGroup =
  | "Node"
  | "Connectivity"
  | "System & safety"
  | "Video & vision"
  | "Device";

const GROUP_ORDER: readonly SettingsGroup[] = [
  "Node",
  "Connectivity",
  "System & safety",
  "Video & vision",
  "Device",
];

interface SectionLink {
  to: string;
  label: string;
  icon: LucideIcon;
  blurb: string;
  group: SettingsGroup;
}

const SECTIONS: SectionLink[] = [
  {
    to: "/settings/profile",
    label: "Profile",
    icon: UserCog,
    blurb: "Drone or ground station, role, restart on apply.",
    group: "Node",
  },
  {
    to: "/settings/region",
    label: "Region",
    icon: Globe,
    blurb: "Operating-region RF posture: unrestricted or pinned.",
    group: "Node",
  },
  {
    to: "/settings/network",
    label: "Network",
    icon: Wifi,
    blurb: "Uplink matrix, failover, Wi-Fi client, hotspot.",
    group: "Connectivity",
  },
  {
    to: "/settings/cellular",
    label: "Cellular",
    icon: Signal,
    blurb: "Modem status, APN, and data cap.",
    group: "Connectivity",
  },
  {
    to: "/settings/mac-pin",
    label: "MAC pinning",
    icon: Fingerprint,
    blurb: "Adapter stability and stable-MAC pins.",
    group: "Connectivity",
  },
  {
    to: "/settings/cloud",
    label: "Cloud",
    icon: Cloud,
    blurb: "Altnautica relay, self-hosted, or local-only.",
    group: "Connectivity",
  },
  {
    to: "/settings/self-heal",
    label: "Self-heal",
    icon: HeartPulse,
    blurb: "Onboard Wi-Fi and camera auto-recovery.",
    group: "System & safety",
  },
  {
    to: "/settings/mavlink",
    label: "MAVLink",
    icon: Route,
    blurb: "FC transport, endpoints, and IDs.",
    group: "System & safety",
  },
  {
    to: "/settings/security",
    label: "Security",
    icon: Lock,
    blurb: "API key, MAVLink WS auth, dashboard PIN.",
    group: "System & safety",
  },
  {
    to: "/settings/discovery",
    label: "Discovery",
    icon: Radar,
    blurb: "Advertised reach names and mDNS.",
    group: "System & safety",
  },
  {
    to: "/settings/vision",
    label: "Vision",
    icon: Eye,
    blurb: "Detection engine, backend, and thresholds.",
    group: "Video & vision",
  },
  {
    to: "/settings/atlas-swarm",
    label: "Atlas & swarm",
    icon: Boxes,
    blurb: "World-model capture and swarm defaults.",
    group: "Video & vision",
  },
  {
    to: "/settings/offload",
    label: "Offload",
    icon: Share2,
    blurb: "Perception offload: drone target or workstation serving.",
    group: "Video & vision",
  },
  {
    to: "/settings/display",
    label: "Display",
    icon: Monitor,
    blurb: "Local kiosk display selection.",
    group: "Device",
  },
  {
    to: "/settings/advanced",
    label: "Advanced",
    icon: Cpu,
    blurb: "Log level and board override.",
    group: "Device",
  },
];

export function SettingsLayout() {
  return (
    <div className="container mx-auto px-4 py-6 max-w-5xl">
      <header className="mb-6 flex items-start gap-3">
        <ShieldAlert className="h-6 w-6 text-warn mt-0.5 shrink-0" />
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">Settings</h1>
          <p className="text-sm text-muted-foreground mt-1">
            Live agent configuration. Risky changes (passwords, profile switches,
            board override) require explicit Save and a confirm prompt.
          </p>
        </div>
      </header>

      <div className="grid grid-cols-1 md:grid-cols-[220px_1fr] gap-6">
        <nav className="space-y-4">
          {GROUP_ORDER.map((group) => {
            const links = SECTIONS.filter((s) => s.group === group);
            if (links.length === 0) return null;
            return (
              <div key={group} className="space-y-1">
                <div className="px-3 text-[10px] font-medium uppercase tracking-wider text-muted-foreground/70">
                  {group}
                </div>
                {links.map((s) => {
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
              </div>
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
