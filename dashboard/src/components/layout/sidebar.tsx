import { ChevronsLeft } from "lucide-react";
import { lazy, Suspense, useState } from "react";
import { useTranslation } from "react-i18next";

import { SidebarNav } from "@/components/layout/sidebar-nav";
import { sidebarAsideClass } from "@/components/layout/sidebar-layout";
import { cn } from "@/lib/utils";
import { ErrorBoundary } from "@/shared/error-boundary";
import { useUiStore } from "@/stores/ui-store";

// The drawer brings the Radix dialog stack; it loads on the first open.
const MobileNavDrawer = lazy(() =>
  import("@/components/layout/mobile-nav-drawer").then((m) => ({ default: m.MobileNavDrawer })),
);

/** The desktop sidebar's collapse button: labelled (w-56) ↔ icons only (w-16),
 *  the state the UI store persists. No hooks, so a test can press it. */
export function SidebarToggle({ collapsed, label }: { collapsed: boolean; label: string }) {
  return (
    <button
      type="button"
      onClick={() => useUiStore.getState().toggleSidebar()}
      aria-label={label}
      aria-expanded={!collapsed}
      title={label}
      className="mt-auto flex min-h-11 shrink-0 items-center justify-center border-t border-border text-muted-foreground transition-colors hover:bg-accent/50 hover:text-foreground"
    >
      <ChevronsLeft className={cn("h-4 w-4 transition-transform", collapsed && "rotate-180")} aria-hidden />
    </button>
  );
}

export function Sidebar() {
  const { t } = useTranslation();
  const collapsed = useUiStore((s) => s.sidebarCollapsed);
  const mobileNavOpen = useUiStore((s) => s.mobileNavOpen);
  // Once loaded the drawer stays mounted, so later opens are instant and
  // Radix sees the open → closed transition.
  const [drawerLoaded, setDrawerLoaded] = useState(false);
  if (mobileNavOpen && !drawerLoaded) setDrawerLoaded(true);

  return (
    <>
      {/* Desktop sidebar — always-visible at lg+, width controlled by collapsed */}
      <aside className={sidebarAsideClass(collapsed)}>
        <SidebarNav collapsed={collapsed} />
        <SidebarToggle collapsed={collapsed} label={collapsed ? t("rail.expandSidebar") : t("rail.collapseSidebar")} />
      </aside>

      {drawerLoaded && (
        <ErrorBoundary fallback={() => <DrawerFailed />}>
          <Suspense fallback={null}>
            <MobileNavDrawer />
          </Suspense>
        </ErrorBoundary>
      )}
    </>
  );
}

/** The drawer could not load (a missing chunk after an update, a dropped
 *  connection) or render: say so while it is asked for, offer a reload, and
 *  leave the page behind it working. */
function DrawerFailed() {
  const { t } = useTranslation();
  const open = useUiStore((s) => s.mobileNavOpen);
  const close = useUiStore((s) => s.closeMobileNav);
  if (!open) return null;
  return (
    <div
      role="alert"
      className="fixed inset-x-3 top-3 z-50 flex items-center justify-between gap-3 rounded-md border border-destructive/40 bg-background px-3 py-2 text-sm text-destructive shadow-lg lg:hidden"
    >
      <span>{t("layout.drawerFailed")}</span>
      <span className="flex gap-2">
        <button type="button" className="min-h-11 rounded-md border border-border px-3 text-foreground" onClick={close}>
          {t("common.close")}
        </button>
        <button
          type="button"
          className="min-h-11 rounded-md bg-primary px-3 text-primary-foreground"
          onClick={() => window.location.reload()}
        >
          {t("layout.reload")}
        </button>
      </span>
    </div>
  );
}
