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
  type LucideIcon,
} from "lucide-react";
import { NavLink } from "react-router-dom";

import { Separator } from "@/components/ui/separator";
import { useStatus } from "@/hooks/use-status";
import type { GroundRole, Profile } from "@/lib/types";
import { cn } from "@/lib/utils";
import { useUiStore } from "@/stores/ui-store";

interface NavItem {
  to: string;
  label: string;
  icon: LucideIcon;
}

const COMMON_TOP: NavItem[] = [{ to: "/", label: "Home", icon: Home }];

const COMMON_BOTTOM: NavItem[] = [
  { to: "/pairing", label: "Pairing", icon: Link2 },
  { to: "/plugins", label: "Plugins", icon: Plug },
  { to: "/peripherals", label: "Peripherals", icon: Cpu },
  { to: "/logs", label: "Logs", icon: ScrollText },
  { to: "/diagnostics", label: "Diagnostics", icon: Wrench },
  { to: "/settings", label: "Settings", icon: SettingsIcon },
];

function droneItems(): NavItem[] {
  return [
    { to: "/telemetry", label: "Telemetry", icon: Radio },
    { to: "/video", label: "Video", icon: Video },
    { to: "/transmit", label: "WFB Transmit", icon: Antenna },
  ];
}

function groundItems(role: GroundRole): NavItem[] {
  const items: NavItem[] = [
    { to: "/receive", label: "WFB Receive", icon: Antenna },
    { to: "/io", label: "Display & Joystick", icon: Gamepad2 },
  ];
  if (role === "relay" || role === "receiver") {
    items.push({ to: "/mesh", label: "Mesh", icon: Network });
  }
  if (role === "receiver") {
    items.push({ to: "/sources", label: "Sources", icon: Layers });
  }
  return items;
}

function itemsForProfile(profile: Profile, role: GroundRole): NavItem[] {
  if (profile === "drone") return droneItems();
  if (profile === "ground_station") return groundItems(role);
  return [];
}

interface SidebarLinkProps {
  item: NavItem;
  collapsed: boolean;
}

function SidebarLink({ item, collapsed }: SidebarLinkProps) {
  const Icon = item.icon;

  return (
    <NavLink
      to={item.to}
      end={item.to === "/"}
      className={({ isActive }) =>
        cn(
          "flex items-center gap-3 px-3 py-2 text-sm rounded-md transition-colors",
          isActive
            ? "bg-accent text-accent-foreground"
            : "text-muted-foreground hover:bg-accent/50 hover:text-foreground",
        )
      }
    >
      <Icon className="h-4 w-4 shrink-0" />
      {!collapsed && <span>{item.label}</span>}
    </NavLink>
  );
}

export function Sidebar() {
  const status = useStatus();
  const collapsed = useUiStore((s) => s.sidebarCollapsed);
  const mobileNavOpen = useUiStore((s) => s.mobileNavOpen);
  const closeMobileNav = useUiStore((s) => s.closeMobileNav);

  const profile: Profile = (status.data?.profile as Profile) ?? "auto";
  const role: GroundRole = status.data?.ground_role ?? "direct";

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
            aria-label="navigation"
          >
            {navList(true)}
          </aside>
        </>
      )}
    </>
  );
}
