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

import type { AllowedProfile } from "@/components/profile-gate";
import { useStatus } from "@/hooks/use-status";
import type { Profile } from "@/lib/types";
import { cn } from "@/lib/utils";

// The settings nav is grouped into two tiers: a small uppercase group header
// over its section links. The nav grew past a dozen entries once the curated
// on-box config pages landed, so a flat list stopped being scannable. The
// group set + order mirrors the GCS node-settings sidebar so the on-box
// dashboard and the GCS reach the same pages under the same headers.
type SettingsGroup =
  | "Identity"
  | "Link & network"
  | "Video & vision"
  | "Cloud & remote"
  | "System & safety";

const GROUP_ORDER: readonly SettingsGroup[] = [
  "Identity",
  "Link & network",
  "Video & vision",
  "Cloud & remote",
  "System & safety",
];

interface SectionLink {
  to: string;
  label: string;
  icon: LucideIcon;
  blurb: string;
  group: SettingsGroup;
  /** Profiles this page applies to. Absent = every profile. Mirrors the GCS
   * node-settings gates and the route's own ProfileGate so the nav never
   * offers a page whose writes are inert on this profile. During the
   * pre-detect ("auto"/"unknown") phase every page stays visible. */
  allow?: AllowedProfile[];
}

/** Whether a section shows on `profile`, matching the ProfileGate convention:
 * ungated pages always show; during pre-detect every page shows; otherwise the
 * concrete profile must be in the allow list. */
function sectionVisible(
  allow: AllowedProfile[] | undefined,
  profile: Profile,
): boolean {
  if (!allow) return true;
  if (profile === "auto" || profile === "unknown") return true;
  return allow.includes(profile as AllowedProfile);
}

const SECTIONS: SectionLink[] = [
  // IDENTITY
  {
    to: "/settings/profile",
    label: "Profile",
    icon: UserCog,
    blurb: "Drone or ground station, role, restart on apply.",
    group: "Identity",
  },
  // LINK & NETWORK
  {
    to: "/settings/network",
    label: "Network",
    icon: Wifi,
    blurb: "Uplink matrix, failover, Wi-Fi client, hotspot.",
    group: "Link & network",
  },
  {
    to: "/settings/cellular",
    label: "Cellular",
    icon: Signal,
    blurb: "Modem status, APN, and data cap.",
    group: "Link & network",
  },
  {
    to: "/settings/mac-pin",
    label: "MAC pinning",
    icon: Fingerprint,
    blurb: "Adapter stability and stable-MAC pins.",
    group: "Link & network",
  },
  {
    to: "/settings/discovery",
    label: "Discovery",
    icon: Radar,
    blurb: "Advertised reach names and mDNS.",
    group: "Link & network",
  },
  {
    to: "/settings/mavlink",
    label: "MAVLink",
    icon: Route,
    blurb: "FC transport, endpoints, and IDs.",
    group: "Link & network",
    // MAVLink routing is the FC-connected drone's surface; a ground station has
    // no MAVLink router to configure.
    allow: ["drone"],
  },
  // VIDEO & VISION
  {
    to: "/settings/vision",
    label: "Vision",
    icon: Eye,
    blurb: "Detection engine, backend, and thresholds.",
    group: "Video & vision",
    // The on-board vision engine runs on the drone; a ground station has none.
    allow: ["drone"],
  },
  {
    to: "/settings/atlas-swarm",
    label: "Atlas & swarm",
    icon: Boxes,
    blurb: "World-model capture and swarm defaults.",
    group: "Video & vision",
    // World-model capture and swarm coordination are drone-fleet surfaces.
    allow: ["drone"],
  },
  {
    to: "/settings/offload",
    label: "Offload",
    icon: Share2,
    blurb: "Perception offload: drone target or workstation serving.",
    group: "Video & vision",
  },
  // CLOUD & REMOTE
  {
    to: "/settings/cloud",
    label: "Cloud",
    icon: Cloud,
    blurb: "Altnautica relay, self-hosted, or local-only.",
    group: "Cloud & remote",
  },
  // SYSTEM & SAFETY
  {
    to: "/settings/region",
    label: "Region",
    icon: Globe,
    blurb: "Operating-region RF posture: unrestricted or pinned.",
    group: "System & safety",
    // The operating region governs the RF radio; only the radio profiles have
    // a regulatory domain to set.
    allow: ["drone", "ground_station"],
  },
  {
    to: "/settings/self-heal",
    label: "Self-heal",
    icon: HeartPulse,
    blurb: "Onboard Wi-Fi and camera auto-recovery.",
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
    to: "/settings/display",
    label: "Display",
    icon: Monitor,
    blurb: "Local kiosk display selection.",
    group: "System & safety",
  },
  {
    to: "/settings/advanced",
    label: "Advanced",
    icon: Cpu,
    blurb: "Log level and board override.",
    group: "System & safety",
  },
];

export function SettingsLayout() {
  const status = useStatus();
  const profile: Profile = (status.data?.profile as Profile) ?? "auto";

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
            const links = SECTIONS.filter(
              (s) => s.group === group && sectionVisible(s.allow, profile),
            );
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
