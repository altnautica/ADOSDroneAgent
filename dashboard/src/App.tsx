import { lazy, Suspense } from "react";
import { Navigate, Route, Routes } from "react-router-dom";

import { DashboardAccessGate } from "@/components/access/DashboardAccessGate";
import { ErrorBoundary } from "@/components/error-boundary";
import { AppShell } from "@/components/layout/app-shell";
import { SettingsLayout } from "@/components/layout/settings-layout";
import { ProfileGate } from "@/components/profile-gate";
import { ThemeProvider } from "@/components/theme-provider";
import { Skeleton } from "@/components/ui/skeleton";
import { TooltipProvider } from "@/components/ui/tooltip";
import { ComingSoonRoute } from "@/routes/coming-soon";
import { DiagnosticsRoute } from "@/routes/diagnostics-route";
import { TransmitRoute } from "@/routes/drone-pages";
import {
  MeshRoute,
  ReceiveRoute,
  SourcesRoute,
} from "@/routes/ground-pages";
import { HomeRoute } from "@/routes/home";
import { IndexRedirect } from "@/routes/index-redirect";
import { IoRoute } from "@/routes/io-route";
import { LogsRoute } from "@/routes/logs-route";
import { OtaRoute } from "@/routes/ota-route";
import { PairingRoute } from "@/routes/pairing-route";
import { PeripheralsRoute } from "@/routes/peripherals-route";
import { AdvancedSettings } from "@/routes/settings/advanced-settings";
import { CellularSettings } from "@/routes/settings/cellular-settings";
import { CloudSettings } from "@/routes/settings/cloud-settings";
import { DisplaySettings } from "@/routes/settings/display-settings";
import { NetworkSettings } from "@/routes/settings/network-settings";
import { OffloadSettings } from "@/routes/settings/offload-settings";
import { ProfileSettings } from "@/routes/settings/profile-settings";
import { RegionSettings } from "@/routes/settings/region-settings";
import { VideoRoute } from "@/routes/video-route";

// Code-split heavy routes. The telemetry page pulls in
// @tanstack/react-virtual + ~600 params; the plugins route pulls in the
// install-dialog + risk-badge bundle. Splitting them keeps the main
// chunk small.
const TelemetryRoute = lazy(() =>
  import("@/routes/telemetry-route").then((m) => ({
    default: m.TelemetryRoute,
  })),
);
const PluginsRoute = lazy(() =>
  import("@/routes/plugins-route").then((m) => ({
    default: m.PluginsRoute,
  })),
);

function RouteFallback() {
  return (
    <div className="p-6 space-y-3">
      <Skeleton className="h-6 w-40" />
      <Skeleton className="h-4 w-64" />
      <Skeleton className="h-32 w-full" />
    </div>
  );
}

export function App() {
  return (
    <ThemeProvider>
      <TooltipProvider delayDuration={200}>
        <ErrorBoundary>
          <DashboardAccessGate>
            <Suspense fallback={<RouteFallback />}>
              <Routes>
              <Route element={<AppShell />}>
                <Route index element={<IndexRedirect />} />
                <Route path="/home" element={<HomeRoute />} />
                <Route path="/pairing" element={<PairingRoute />} />
                <Route
                  path="/receive"
                  element={
                    <ProfileGate allow={["ground_station"]}>
                      <ReceiveRoute />
                    </ProfileGate>
                  }
                />
                <Route
                  path="/mesh"
                  element={
                    <ProfileGate
                      allow={["ground_station"]}
                      roles={["relay", "receiver"]}
                    >
                      <MeshRoute />
                    </ProfileGate>
                  }
                />
                <Route
                  path="/sources"
                  element={
                    <ProfileGate
                      allow={["ground_station"]}
                      roles={["receiver"]}
                    >
                      <SourcesRoute />
                    </ProfileGate>
                  }
                />
                <Route path="/plugins" element={<PluginsRoute />} />
                <Route path="/peripherals" element={<PeripheralsRoute />} />
                <Route path="/ota" element={<OtaRoute />} />
                <Route path="/logs" element={<LogsRoute />} />
                <Route path="/diagnostics" element={<DiagnosticsRoute />} />
                <Route
                  path="/telemetry"
                  element={
                    <ProfileGate allow={["drone"]}>
                      <TelemetryRoute />
                    </ProfileGate>
                  }
                />
                <Route
                  path="/video"
                  element={
                    <ProfileGate allow={["drone"]}>
                      <VideoRoute />
                    </ProfileGate>
                  }
                />
                <Route
                  path="/transmit"
                  element={
                    <ProfileGate allow={["drone"]}>
                      <TransmitRoute />
                    </ProfileGate>
                  }
                />
                <Route
                  path="/io"
                  element={
                    <ProfileGate allow={["ground_station"]}>
                      <IoRoute />
                    </ProfileGate>
                  }
                />
                <Route path="/settings" element={<SettingsLayout />}>
                  <Route index element={<Navigate to="profile" replace />} />
                  <Route path="profile" element={<ProfileSettings />} />
                  <Route path="region" element={<RegionSettings />} />
                  <Route path="network" element={<NetworkSettings />} />
                  <Route path="cellular" element={<CellularSettings />} />
                  <Route path="cloud" element={<CloudSettings />} />
                  <Route path="display" element={<DisplaySettings />} />
                  <Route path="offload" element={<OffloadSettings />} />
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
            </Suspense>
          </DashboardAccessGate>
        </ErrorBoundary>
      </TooltipProvider>
    </ThemeProvider>
  );
}
