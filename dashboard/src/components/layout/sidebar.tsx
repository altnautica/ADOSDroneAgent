import * as DialogPrimitive from "@radix-ui/react-dialog";
import { useTranslation } from "react-i18next";
import { NavLink } from "react-router-dom";

import { Separator } from "@/components/ui/separator";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import { navGroups, type NavItem } from "@/lib/nav-items";
import { cn } from "@/lib/utils";
import { useUiStore } from "@/stores/ui-store";

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

export function Sidebar() {
  const { t } = useTranslation();
  const { profile, role } = useNodeIdentity();
  const collapsed = useUiStore((s) => s.sidebarCollapsed);
  const mobileNavOpen = useUiStore((s) => s.mobileNavOpen);
  const closeMobileNav = useUiStore((s) => s.closeMobileNav);

  const { top, profileItems, bottom } = navGroups(profile, role);

  // The mobile drawer closes on any click inside its nav (see navList).

  const navList = (mobileMode: boolean) => (
    <nav
      className="flex-1 py-3 px-2 space-y-0.5 overflow-y-auto"
      onClick={mobileMode ? closeMobileNav : undefined}
    >
      {top.map((item) => (
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
      {bottom.map((item) => (
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

      {/* Mobile drawer below lg: a modal dialog, so focus moves in and stays
          trapped, Escape closes it, and it reads as a dialog. */}
      <DialogPrimitive.Root open={mobileNavOpen} onOpenChange={(open) => !open && closeMobileNav()}>
        <DialogPrimitive.Portal>
          <DialogPrimitive.Overlay className="lg:hidden fixed inset-0 z-40 bg-black/60 backdrop-blur-sm" />
          <DialogPrimitive.Content
            className="lg:hidden fixed inset-y-0 left-0 z-50 flex w-64 flex-col border-r border-border bg-background focus:outline-none"
            aria-describedby={undefined}
          >
            <DialogPrimitive.Title className="sr-only">{t("nav.navigation")}</DialogPrimitive.Title>
            {navList(true)}
          </DialogPrimitive.Content>
        </DialogPrimitive.Portal>
      </DialogPrimitive.Root>
    </>
  );
}
