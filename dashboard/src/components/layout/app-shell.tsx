import { Suspense, useEffect } from "react";
import { Outlet } from "react-router-dom";
import { Toaster, toast as sonnerToast } from "sonner";

import { BannerHost } from "./banner-host";
import { BottomDock } from "./bottom-dock";
import { Header } from "./header";
import { Sidebar } from "./sidebar";
import { RouteFallback } from "@/components/route-fallback";
import { DirtyGuardHost } from "@/hooks/use-dirty-guard";
import { onToast, type ToastMessage } from "@/shared/api/toast";
import { useUiStore } from "@/stores/ui-store";

/** Draw one message from the shared toast bus with sonner. */
function showToast({ kind, message, description }: ToastMessage): void {
  const opts = description ? { description } : undefined;
  if (kind === "ok") sonnerToast.success(message, opts);
  else if (kind === "err") sonnerToast.error(message, opts);
  else sonnerToast(message, opts);
}

export function AppShell() {
  const theme = useUiStore((s) => s.theme);
  // The shell is the one toast host: every screen posts to the shared bus.
  useEffect(() => onToast(showToast), []);
  return (
    <div className="flex flex-col min-h-dvh">
      <DirtyGuardHost />
      <Header />
      <div className="flex-1 flex">
        <Sidebar />
        <main className="flex-1 min-w-0">
          <BannerHost />
          <div className="px-4 lg:px-6 py-4 lg:py-6">
            <Suspense fallback={<RouteFallback />}>
              <Outlet />
            </Suspense>
          </div>
        </main>
      </div>
      <BottomDock />
      <Toaster
        position="top-right"
        duration={3000}
        theme={theme === "dark" ? "dark" : theme === "light" ? "light" : "system"}
        toastOptions={{
          classNames: {
            toast:
              "border border-border bg-background text-foreground shadow-lg",
            description: "text-muted-foreground",
          },
        }}
      />
    </div>
  );
}
