import {
  BatteryMedium,
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
  ShieldAlert,
  Signal,
  UserCog,
  Wifi,
  type LucideIcon,
} from "lucide-react";
import { Suspense } from "react";
import type { ParseKeys } from "i18next";
import { useTranslation } from "react-i18next";
import { NavLink, Outlet } from "react-router-dom";

import type { AllowedProfile } from "@/components/profile-gate";
import { RouteFallback } from "@/components/route-fallback";
import { useNodeProfile } from "@/hooks/use-node-profile";
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

const GROUP_LABEL_KEYS: Record<SettingsGroup, ParseKeys> = {
  "Identity": "layout.settings.groupIdentity",
  "Link & network": "layout.settings.groupLink",
  "Video & vision": "layout.settings.groupVideo",
  "Cloud & remote": "layout.settings.groupCloud",
  "System & safety": "layout.settings.groupSystem",
};

interface SectionLink {
  to: string;
  labelKey: ParseKeys;
  icon: LucideIcon;
  blurbKey: ParseKeys;
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
    labelKey: "nav.profile",
    icon: UserCog,
    blurbKey: "layout.settings.profileBlurb",
    group: "Identity",
  },
  // LINK & NETWORK
  {
    to: "/settings/network",
    labelKey: "nav.network",
    icon: Wifi,
    blurbKey: "layout.settings.networkBlurb",
    group: "Link & network",
  },
  {
    to: "/settings/cellular",
    labelKey: "nav.cellular",
    icon: Signal,
    blurbKey: "layout.settings.cellularBlurb",
    group: "Link & network",
  },
  {
    to: "/settings/mac-pin",
    labelKey: "nav.macPin",
    icon: Fingerprint,
    blurbKey: "layout.settings.macPinBlurb",
    group: "Link & network",
  },
  {
    to: "/settings/discovery",
    labelKey: "nav.discovery",
    icon: Radar,
    blurbKey: "layout.settings.discoveryBlurb",
    group: "Link & network",
  },
  {
    to: "/settings/mavlink",
    labelKey: "nav.mavlink",
    icon: Route,
    blurbKey: "layout.settings.mavlinkBlurb",
    group: "Link & network",
    // MAVLink routing is the FC-connected drone's surface; a ground station has
    // no MAVLink router to configure.
    allow: ["drone"],
  },
  // VIDEO & VISION
  {
    to: "/settings/vision",
    labelKey: "nav.vision",
    icon: Eye,
    blurbKey: "layout.settings.visionBlurb",
    group: "Video & vision",
    // The on-board vision engine runs on the drone; a ground station has none.
    allow: ["drone"],
  },
  {
    to: "/settings/camera",
    labelKey: "nav.camera",
    icon: Monitor,
    blurbKey: "layout.settings.cameraBlurb",
    group: "Video & vision",
    // Only the drone owns an encode pipeline for its onboard camera.
    allow: ["drone"],
  },
  {
    to: "/settings/swarm",
    labelKey: "nav.swarm",
    icon: Boxes,
    blurbKey: "layout.settings.swarmBlurb",
    group: "Video & vision",
    // Swarm coordination is a drone-fleet surface.
    allow: ["drone"],
  },
  // CLOUD & REMOTE
  {
    to: "/settings/cloud",
    labelKey: "nav.cloud",
    icon: Cloud,
    blurbKey: "layout.settings.cloudBlurb",
    group: "Cloud & remote",
  },
  // SYSTEM & SAFETY
  {
    to: "/settings/region",
    labelKey: "nav.region",
    icon: Globe,
    blurbKey: "layout.settings.regionBlurb",
    group: "System & safety",
    // The operating region governs the RF radio; only the radio profiles have
    // a regulatory domain to set.
    allow: ["drone", "ground_station"],
  },
  {
    to: "/settings/battery",
    labelKey: "nav.battery",
    icon: BatteryMedium,
    blurbKey: "layout.settings.batteryBlurb",
    group: "System & safety",
    // Battery telemetry comes from the flight controller the drone carries.
    allow: ["drone"],
  },
  {
    to: "/settings/self-heal",
    labelKey: "nav.selfHeal",
    icon: HeartPulse,
    blurbKey: "layout.settings.selfHealBlurb",
    group: "System & safety",
  },
  {
    to: "/settings/security",
    labelKey: "nav.security",
    icon: Lock,
    blurbKey: "layout.settings.securityBlurb",
    group: "System & safety",
  },
  {
    to: "/settings/display",
    labelKey: "nav.display",
    icon: Monitor,
    blurbKey: "layout.settings.displayBlurb",
    group: "System & safety",
    allow: ["ground_station"],
  },
  {
    to: "/settings/advanced",
    labelKey: "nav.advanced",
    icon: Cpu,
    blurbKey: "layout.settings.advancedBlurb",
    group: "System & safety",
  },
];

export function SettingsLayout() {
  const { t } = useTranslation();
  const profile = useNodeProfile();
  // Settings sections read the setup status; the layout keeps it fresh while
  // any settings page is open.
  useStatus({ poll: true });

  return (
    <div className="container mx-auto px-4 py-6 max-w-5xl">
      <header className="mb-6 flex items-start gap-3">
        <ShieldAlert className="h-6 w-6 text-warn mt-0.5 shrink-0" />
        <div>
          <h1 className="text-2xl font-semibold tracking-tight">{t("layout.settings.title")}</h1>
          <p className="text-sm text-muted-foreground mt-1">
            {t("layout.settings.intro")}
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
                  {t(GROUP_LABEL_KEYS[group])}
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
                        <div className="font-medium">{t(s.labelKey)}</div>
                        <div className="text-[11px] text-muted-foreground/80 mt-0.5">
                          {t(s.blurbKey)}
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
          <Suspense fallback={<RouteFallback />}>
            <Outlet />
          </Suspense>
        </main>
      </div>
    </div>
  );
}
