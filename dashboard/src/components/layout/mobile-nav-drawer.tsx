import * as DialogPrimitive from "@radix-ui/react-dialog";
import { useTranslation } from "react-i18next";

import { SidebarNav } from "@/components/layout/sidebar-nav";
import { useUiStore } from "@/stores/ui-store";

/**
 * Mobile nav drawer below lg: a modal dialog, so focus moves in and stays
 * trapped, Escape closes it, and it reads as a dialog. The sidebar loads
 * this module on the first open, so the dialog code stays out of the main
 * chunk.
 */
export function MobileNavDrawer() {
  const { t } = useTranslation();
  const mobileNavOpen = useUiStore((s) => s.mobileNavOpen);
  const closeMobileNav = useUiStore((s) => s.closeMobileNav);

  return (
    <DialogPrimitive.Root open={mobileNavOpen} onOpenChange={(open) => !open && closeMobileNav()}>
      <DialogPrimitive.Portal>
        <DialogPrimitive.Overlay className="lg:hidden fixed inset-0 z-40 bg-black/60 backdrop-blur-sm" />
        <DialogPrimitive.Content
          className="lg:hidden fixed inset-y-0 left-0 z-50 flex w-64 flex-col border-r border-border bg-background focus:outline-none"
          aria-describedby={undefined}
        >
          <DialogPrimitive.Title className="sr-only">{t("nav.navigation")}</DialogPrimitive.Title>
          <SidebarNav collapsed={false} onNavigate={closeMobileNav} />
        </DialogPrimitive.Content>
      </DialogPrimitive.Portal>
    </DialogPrimitive.Root>
  );
}
