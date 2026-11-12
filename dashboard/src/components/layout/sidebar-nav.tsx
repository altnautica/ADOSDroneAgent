import { useTranslation } from "react-i18next";
import { NavLink } from "react-router-dom";

import { Separator } from "@/components/ui/separator";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import { navGroups, type NavItem } from "@/lib/nav-items";
import { cn } from "@/lib/utils";

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
          "flex items-center gap-3 px-3 py-2 text-sm rounded-md transition-colors max-md:min-h-11",
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

interface SidebarNavProps {
  collapsed: boolean;
  /** Called on any click inside the list; the mobile drawer closes on it. */
  onNavigate?: () => void;
}

/** The nav link list shared by the desktop sidebar and the mobile drawer. */
export function SidebarNav({ collapsed, onNavigate }: SidebarNavProps) {
  const { profile, role } = useNodeIdentity();
  const { top, profileItems, bottom } = navGroups(profile, role);

  return (
    <nav className="flex-1 py-3 px-2 space-y-0.5 overflow-y-auto" onClick={onNavigate}>
      {top.map((item) => (
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
      {bottom.map((item) => (
        <SidebarLink key={item.to} item={item} collapsed={collapsed} />
      ))}
    </nav>
  );
}
