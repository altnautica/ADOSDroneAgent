import { Outlet } from "react-router-dom";
import { Toaster } from "sonner";

import { BannerHost } from "./banner-host";
import { BottomDock } from "./bottom-dock";
import { Header } from "./header";
import { Sidebar } from "./sidebar";
import { useUiStore } from "@/stores/ui-store";

export function AppShell() {
  const theme = useUiStore((s) => s.theme);
  return (
    <div className="flex flex-col min-h-dvh">
      <Header />
      <div className="flex-1 flex">
        <Sidebar />
        <main className="flex-1 min-w-0">
          <BannerHost />
          <div className="px-4 lg:px-6 py-4 lg:py-6">
            <Outlet />
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
