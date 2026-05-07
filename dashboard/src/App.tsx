import { Navigate, Route, Routes } from "react-router-dom";

import { AppShell } from "@/components/layout/app-shell";
import { ThemeProvider } from "@/components/theme-provider";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ComingSoonRoute } from "@/routes/coming-soon";
import { HomeRoute } from "@/routes/home";

export function App() {
  return (
    <ThemeProvider>
      <TooltipProvider delayDuration={200}>
        <Routes>
          <Route element={<AppShell />}>
            <Route index element={<HomeRoute />} />
            <Route
              path="/setup"
              element={
                <ComingSoonRoute
                  title="Setup"
                  description="The 4-step setup wizard rebuild lands next."
                  shipsIn="v0.14.2"
                />
              }
            />
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
