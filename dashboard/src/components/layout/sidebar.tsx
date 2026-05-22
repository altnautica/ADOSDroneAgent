import {
  Home,
  Radio,
  Video,
  Link2,
  Plug,
  Cpu,
  ArrowUpFromLine,
  ScrollText,
  Wrench,
  Settings as SettingsIcon,
  Bot,
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
  enabled: boolean;
}

const COMMON_TOP: NavItem[] = [{ to: "/", label: "Home", icon: Home, enabled: true }];

const COMMON_BOTTOM: NavItem[] = [
  { to: "/pairing", label: "Pairing", icon: Link2, enabled: true },
  { to: "/plugins", label: "Plugins", icon: Plug, enabled: true },
  { to: "/peripherals", label: "Peripherals", icon: Cpu, enabled: true },
  { to: "/ota", label: "Updates", icon: ArrowUpFromLine, enabled: true },
  { to: "/logs", label: "Logs", icon: ScrollText, enabled: true },
  { to: "/diagnostics", label: "Diagnostics", icon: Wrench, enabled: true },
  { to: "/settings", label: "Settings", icon: SettingsIcon, enabled: true },
];

function droneItems(): NavItem[] {
  // ROS is always linked — the route itself shows an "install ROS"
  // empty state when the overlay isn't present, which is friendlier
  // than hiding the link and leaving operators wondering where to go.
  return [
    { to: "/telemetry", label: "Telemetry", icon: Radio, enabled: true },
    { to: "/video", label: "Video", icon: Video, enabled: true },
    { to: "/ros", label: "ROS", icon: Bot, enabled: true },
  ];
}

function groundItems(role: GroundRole): NavItem[] {
  const items: NavItem[] = [
    { to: "/receive", label: "Receive", icon: Antenna, enabled: true },
    { to: "/io", label: "Display & Joystick", icon: Gamepad2, enabled: true },
  ];
  if (role === "relay" || role === "receiver") {
    items.push({ to: "/mesh", label: "Mesh", icon: Network, enabled: true });
  }
  if (role === "receiver") {
    items.push({ to: "/sources", label: "Sources", icon: Layers, enabled: true });
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

  if (!item.enabled) {
    return (
      <div
        className={cn(
          "flex items-center gap-3 px-3 py-2 text-sm rounded-md",
          "text-muted-foreground/50 cursor-not-allowed select-none",
        )}
        title={collapsed ? `${item.label} — coming soon` : "coming soon"}
      >
        <Icon className="h-4 w-4 shrink-0" />
        {!collapsed && (
          <>
            <span className="flex-1">{item.label}</span>
            <span className="text-[9px] uppercase tracking-wider opacity-60">
              soon
            </span>
          </>
        )}
      </div>
    );
  }

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

  // Auto-close the mobile drawer on navigation (NavLink click).
  // Implementation: every SidebarLink calls closeMobileNav on click.

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
