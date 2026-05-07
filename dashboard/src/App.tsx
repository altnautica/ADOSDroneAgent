import { Navigate, Route, Routes } from "react-router-dom";

import { AppShell } from "@/components/layout/app-shell";
import { SettingsLayout } from "@/components/layout/settings-layout";
import { ThemeProvider } from "@/components/theme-provider";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ComingSoonRoute } from "@/routes/coming-soon";
import { HomeRoute } from "@/routes/home";
import { IndexRedirect } from "@/routes/index-redirect";
import { PairingRoute } from "@/routes/pairing-route";
import { SetupRoute } from "@/routes/setup-route";
import { AdvancedSettings } from "@/routes/settings/advanced-settings";
import { CloudSettings } from "@/routes/settings/cloud-settings";
import { DisplaySettings } from "@/routes/settings/display-settings";
import { NetworkSettings } from "@/routes/settings/network-settings";
import { ProfileSettings } from "@/routes/settings/profile-settings";

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
            <Route path="/settings" element={<SettingsLayout />}>
              <Route index element={<Navigate to="profile" replace />} />
              <Route path="profile" element={<ProfileSettings />} />
              <Route path="network" element={<NetworkSettings />} />
              <Route path="cloud" element={<CloudSettings />} />
              <Route path="display" element={<DisplaySettings />} />
              <Route path="advanced" element={<AdvancedSettings />} />
            </Route>
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
