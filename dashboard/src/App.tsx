import { Navigate, Route, Routes } from "react-router-dom";

import { AppShell } from "@/components/layout/app-shell";
import { ThemeProvider } from "@/components/theme-provider";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ComingSoonRoute } from "@/routes/coming-soon";
import { HomeRoute } from "@/routes/home";
import { IndexRedirect } from "@/routes/index-redirect";
import { PairingRoute } from "@/routes/pairing-route";
import { SetupRoute } from "@/routes/setup-route";

export function App() {
  return (
    <ThemeProvider>
      <TooltipProvider delayDuration={200}>
        <Routes>
          <Route element={<AppShell />}>
            <Route index element={<IndexRedirect />} />
            <Route path="/home" element={<HomeRoute />} />
            <Route path="/setup" element={<SetupRoute />} />
            <Route path="/pairing" element={<PairingRoute />} />
            <Route
              path="/settings"
              element={
                <ComingSoonRoute
                  title="Settings"
                  description="Profile, network, cloud, display, and advanced sections."
                  shipsIn="v0.14.3"
                />
              }
            />
            <Route
              path="*"
              element={
                <ComingSoonRoute
                  title="Coming soon"
                  description="This page hasn't been wired into the new dashboard yet."
                />
              }
            />
          </Route>
          <Route path="*" element={<Navigate to="/" replace />} />
        </Routes>
      </TooltipProvider>
    </ThemeProvider>
  );
}
