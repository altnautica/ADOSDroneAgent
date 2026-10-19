import {
  Home,
  Radio,
  Video,
  Link2,
  Plug,
  Cpu,
  ScrollText,
  Wrench,
  Settings as SettingsIcon,
  Antenna,
  Network,
  Layers,
  Gamepad2,
  Activity,
  Aperture,
  Satellite,
  RadioTower,
  type LucideIcon,
} from "lucide-react";
import type { ParseKeys } from "i18next";
import { useTranslation } from "react-i18next";
import { NavLink } from "react-router-dom";

import { Separator } from "@/components/ui/separator";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import type { GroundRole, Profile } from "@/lib/types";
import { cn } from "@/lib/utils";
import { useUiStore } from "@/stores/ui-store";

interface NavItem {
  to: string;
  labelKey: ParseKeys;
  icon: LucideIcon;
}

const COMMON_TOP: NavItem[] = [{ to: "/", labelKey: "nav.home", icon: Home }];

const COMMON_BOTTOM: NavItem[] = [
  { to: "/pairing", labelKey: "nav.pairing", icon: Link2 },
  { to: "/extensions", labelKey: "nav.extensions", icon: Plug },
  { to: "/peripherals", labelKey: "nav.peripherals", icon: Cpu },
  { to: "/logs", labelKey: "nav.logs", icon: ScrollText },
  { to: "/diagnostics", labelKey: "nav.diagnostics", icon: Wrench },
  { to: "/settings", labelKey: "nav.settings", icon: SettingsIcon },
];

function droneItems(): NavItem[] {
  return [
    { to: "/telemetry", labelKey: "nav.telemetry", icon: Radio },
    { to: "/video", labelKey: "nav.video", icon: Video },
    { to: "/transmit", labelKey: "nav.transmit", icon: Antenna },
    { to: "/sensors", labelKey: "nav.sensors", icon: Activity },
    { to: "/lora", labelKey: "nav.lora", icon: RadioTower },
  ];
}

// A visor has cameras and a sensor hub but no long-range radio to transmit on.
function visorItems(): NavItem[] {
  return [
    { to: "/telemetry", labelKey: "nav.telemetry", icon: Radio },
    { to: "/video", labelKey: "nav.video", icon: Video },
    { to: "/capture", labelKey: "nav.capture", icon: Aperture },
    { to: "/sensors", labelKey: "nav.sensors", icon: Activity },
  ];
}

function groundItems(role: GroundRole): NavItem[] {
  const items: NavItem[] = [
    { to: "/receive", labelKey: "nav.receive", icon: Antenna },
    { to: "/io", labelKey: "nav.io", icon: Gamepad2 },
    { to: "/gnss", labelKey: "nav.gnss", icon: Satellite },
    { to: "/lora", labelKey: "nav.lora", icon: RadioTower },
  ];
  if (role === "relay" || role === "receiver") {
    items.push({ to: "/mesh", labelKey: "nav.mesh", icon: Network });
  }
  if (role === "receiver") {
    items.push({ to: "/sources", labelKey: "nav.sources", icon: Layers });
  }
  return items;
}

function itemsForProfile(profile: Profile, role: GroundRole): NavItem[] {
  if (profile === "drone") return droneItems();
  if (profile === "ground_station") return groundItems(role);
  if (profile === "visor") return visorItems();
  return [];
}

interface SidebarLinkProps {
  item: NavItem;
  collapsed: boolean;
}

function SidebarLink({ item, collapsed }: SidebarLinkProps) {
  const { t } = useTranslation();
  const Icon = item.icon;
  const label = t(item.labelKey);

  const link = (
    <NavLink
      to={item.to}
      end={item.to === "/"}
      aria-label={collapsed ? label : undefined}
      className={({ isActive }) =>
        cn(
          "flex items-center gap-3 px-3 py-2 text-sm rounded-md transition-colors",
          isActive
            ? "bg-accent text-accent-foreground"
            : "text-muted-foreground hover:bg-accent/50 hover:text-foreground",
        )
      }
    >
      <Icon className="h-4 w-4 shrink-0" aria-hidden />
      {!collapsed && <span>{label}</span>}
    </NavLink>
  );

  if (!collapsed) return link;
  return (
    <Tooltip>
      <TooltipTrigger asChild>{link}</TooltipTrigger>
      <TooltipContent side="right">{label}</TooltipContent>
    </Tooltip>
  );
}

export function Sidebar() {
  const { t } = useTranslation();
  const { profile, role } = useNodeIdentity();
  const collapsed = useUiStore((s) => s.sidebarCollapsed);
  const mobileNavOpen = useUiStore((s) => s.mobileNavOpen);
  const closeMobileNav = useUiStore((s) => s.closeMobileNav);

  const profileItems = itemsForProfile(profile, role);

  // The mobile drawer closes on any click inside its nav (see navList).

  const navList = (mobileMode: boolean) => (
    <nav
      className="flex-1 py-3 px-2 space-y-0.5 overflow-y-auto"
      onClick={mobileMode ? closeMobileNav : undefined}
    >
      {COMMON_TOP.map((item) => (
        <SidebarLink
          key={item.to}
          item={item}
          collapsed={mobileMode ? false : collapsed}
        />
      ))}

      {profileItems.length > 0 && (
        <>
          <Separator className="my-2" />
          {profileItems.map((item) => (
            <SidebarLink
              key={item.to}
              item={item}
              collapsed={mobileMode ? false : collapsed}
            />
          ))}
        </>
      )}

      <Separator className="my-2" />
      {COMMON_BOTTOM.map((item) => (
        <SidebarLink
          key={item.to}
          item={item}
          collapsed={mobileMode ? false : collapsed}
        />
      ))}
    </nav>
  );

  return (
    <>
      {/* Desktop sidebar — always-visible at lg+, width controlled by collapsed */}
      <aside
        className={cn(
          "hidden lg:flex flex-col border-r border-border bg-background/40 transition-[width]",
          collapsed ? "w-16" : "w-56",
        )}
      >
        {navList(false)}
      </aside>

      {/* Mobile drawer — overlay below lg, controlled by mobileNavOpen */}
      {mobileNavOpen && (
        <>
          <div
            className="lg:hidden fixed inset-0 z-40 bg-black/60 backdrop-blur-sm"
            aria-hidden
            onClick={closeMobileNav}
          />
          <aside
            className="lg:hidden fixed inset-y-0 left-0 z-50 flex w-64 flex-col border-r border-border bg-background"
            aria-label={t("nav.navigation")}
          >
            {navList(true)}
          </aside>
        </>
      )}
    </>
  );
}
