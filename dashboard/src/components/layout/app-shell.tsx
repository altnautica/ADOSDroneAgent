import { Outlet } from "react-router-dom";

import { BannerHost } from "./banner-host";
import { BottomDock } from "./bottom-dock";
import { Header } from "./header";
import { Sidebar } from "./sidebar";

export function AppShell() {
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
    </div>
  );
}
