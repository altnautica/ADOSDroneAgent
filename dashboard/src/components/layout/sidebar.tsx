import {
  Home,
  Radio,
  Video,
  Link2,
  Plug,
  Cpu,
  Package,
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
  { to: "/plugins", label: "Plugins", icon: Plug, enabled: false },
  { to: "/peripherals", label: "Peripherals", icon: Cpu, enabled: false },
  { to: "/suites", label: "Suites", icon: Package, enabled: false },
  { to: "/ota", label: "Updates", icon: ArrowUpFromLine, enabled: false },
  { to: "/logs", label: "Logs", icon: ScrollText, enabled: false },
  { to: "/diagnostics", label: "Diagnostics", icon: Wrench, enabled: false },
  { to: "/settings", label: "Settings", icon: SettingsIcon, enabled: false },
];

function droneItems(rosInstalled: boolean): NavItem[] {
  return [
    { to: "/telemetry", label: "Telemetry", icon: Radio, enabled: false },
    { to: "/video", label: "Video", icon: Video, enabled: false },
    ...(rosInstalled
      ? [{ to: "/ros", label: "ROS", icon: Bot, enabled: false } as NavItem]
      : []),
  ];
}

function groundItems(role: GroundRole): NavItem[] {
  const items: NavItem[] = [
    { to: "/receive", label: "Receive", icon: Antenna, enabled: false },
    { to: "/io", label: "Display & Joystick", icon: Gamepad2, enabled: false },
  ];
  if (role === "relay" || role === "receiver") {
    items.push({ to: "/mesh", label: "Mesh", icon: Network, enabled: false });
  }
  if (role === "receiver") {
    items.push({ to: "/sources", label: "Sources", icon: Layers, enabled: false });
  }
  return items;
}

function itemsForProfile(
  profile: Profile,
  role: GroundRole,
  rosInstalled: boolean,
): NavItem[] {
  if (profile === "drone") return droneItems(rosInstalled);
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

  const profile: Profile = (status.data?.profile as Profile) ?? "auto";
  const role: GroundRole = status.data?.ground_role ?? "direct";
  const rosInstalled = false;

  const profileItems = itemsForProfile(profile, role, rosInstalled);

  return (
    <aside
      className={cn(
        "hidden lg:flex flex-col border-r border-border bg-background/40 transition-[width]",
        collapsed ? "w-16" : "w-56",
      )}
    >
      <nav className="flex-1 py-3 px-2 space-y-0.5 overflow-y-auto">
        {COMMON_TOP.map((item) => (
          <SidebarLink key={item.to} item={item} collapsed={collapsed} />
        ))}

        {profileItems.length > 0 && (
          <>
            <Separator className="my-2" />
            {profileItems.map((item) => (
              <SidebarLink key={item.to} item={item} collapsed={collapsed} />
            ))}
          </>
        )}

        <Separator className="my-2" />
        {COMMON_BOTTOM.map((item) => (
          <SidebarLink key={item.to} item={item} collapsed={collapsed} />
        ))}
      </nav>
    </aside>
  );
}
