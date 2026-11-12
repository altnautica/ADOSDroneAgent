import { lazy, Suspense, useState } from "react";

import { SidebarNav } from "@/components/layout/sidebar-nav";
import { cn } from "@/lib/utils";
import { useUiStore } from "@/stores/ui-store";

// The drawer brings the Radix dialog stack; it loads on the first open.
const MobileNavDrawer = lazy(() =>
  import("@/components/layout/mobile-nav-drawer").then((m) => ({ default: m.MobileNavDrawer })),
);

export function Sidebar() {
  const collapsed = useUiStore((s) => s.sidebarCollapsed);
  const mobileNavOpen = useUiStore((s) => s.mobileNavOpen);
  // Once loaded the drawer stays mounted, so later opens are instant and
  // Radix sees the open → closed transition.
  const [drawerLoaded, setDrawerLoaded] = useState(false);
  if (mobileNavOpen && !drawerLoaded) setDrawerLoaded(true);

  return (
    <>
      {/* Desktop sidebar — always-visible at lg+, width controlled by collapsed */}
      <aside
        className={cn(
          "hidden lg:flex flex-col border-r border-border bg-background/40 transition-[width]",
          collapsed ? "w-16" : "w-56",
        )}
      >
        <SidebarNav collapsed={collapsed} />
      </aside>

      {drawerLoaded && (
        <Suspense fallback={null}>
          <MobileNavDrawer />
        </Suspense>
      )}
    </>
  );
}
