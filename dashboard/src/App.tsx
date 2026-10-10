import { lazy, Suspense } from "react";
import { Navigate, Route, Routes } from "react-router-dom";

import { DashboardAccessGate } from "@/components/access/DashboardAccessGate";
import { ErrorBoundary } from "@/shared/error-boundary";
import { AppShell } from "@/components/layout/app-shell";
import { SettingsLayout } from "@/components/layout/settings-layout";
import { ProfileGate } from "@/components/profile-gate";
import { ThemeProvider } from "@/components/theme-provider";
import { RouteFallback } from "@/components/route-fallback";
import { TooltipProvider } from "@/components/ui/tooltip";
import { HomeRoute } from "@/routes/home";
import { IndexRedirect } from "@/routes/index-redirect";
import { NotFoundRoute } from "@/routes/not-found";

// Every route except Home is code-split so the main chunk carries only the
// shell and the landing page. Each page loads on first navigation.
const DiagnosticsRoute = lazy(() =>
  import("@/routes/diagnostics-route").then((m) => ({ default: m.DiagnosticsRoute })),
);
const TransmitRoute = lazy(() =>
  import("@/routes/drone-pages").then((m) => ({ default: m.TransmitRoute })),
);
const MeshRoute = lazy(() =>
  import("@/routes/ground-pages").then((m) => ({ default: m.MeshRoute })),
);
const ReceiveRoute = lazy(() =>
  import("@/routes/ground-pages").then((m) => ({ default: m.ReceiveRoute })),
);
const SourcesRoute = lazy(() =>
  import("@/routes/ground-pages").then((m) => ({ default: m.SourcesRoute })),
);
const IoRoute = lazy(() =>
  import("@/routes/io-route").then((m) => ({ default: m.IoRoute })),
);
const LogsRoute = lazy(() =>
  import("@/routes/logs-route").then((m) => ({ default: m.LogsRoute })),
);
const PairingRoute = lazy(() =>
  import("@/routes/pairing-route").then((m) => ({ default: m.PairingRoute })),
);
const PeripheralsRoute = lazy(() =>
  import("@/routes/peripherals-route").then((m) => ({ default: m.PeripheralsRoute })),
);
const ExtensionsRoute = lazy(() =>
  import("@/routes/extensions-route").then((m) => ({ default: m.ExtensionsRoute })),
);
const TelemetryRoute = lazy(() =>
  import("@/routes/telemetry-route").then((m) => ({ default: m.TelemetryRoute })),
);
const VideoRoute = lazy(() =>
  import("@/routes/video-route").then((m) => ({ default: m.VideoRoute })),
);
const AdvancedSettings = lazy(() =>
  import("@/routes/settings/advanced-settings").then((m) => ({
    default: m.AdvancedSettings,
  })),
);
const BatterySettings = lazy(() =>
  import("@/routes/settings/battery-settings").then((m) => ({
    default: m.BatterySettings,
  })),
);
const CellularSettings = lazy(() =>
  import("@/routes/settings/cellular-settings").then((m) => ({
    default: m.CellularSettings,
  })),
);
const CameraSettings = lazy(() =>
  import("@/routes/settings/camera-settings").then((m) => ({
    default: m.CameraSettings,
  })),
);
const CloudSettings = lazy(() =>
  import("@/routes/settings/cloud-settings").then((m) => ({
    default: m.CloudSettings,
  })),
);
const DiscoverySettings = lazy(() =>
  import("@/routes/settings/discovery-settings").then((m) => ({
    default: m.DiscoverySettings,
  })),
);
const DisplaySettings = lazy(() =>
  import("@/routes/settings/display-settings").then((m) => ({
    default: m.DisplaySettings,
  })),
);
const MacPinSettings = lazy(() =>
  import("@/routes/settings/mac-pin-settings").then((m) => ({
    default: m.MacPinSettings,
  })),
);
const MavlinkSettings = lazy(() =>
  import("@/routes/settings/mavlink-settings").then((m) => ({
    default: m.MavlinkSettings,
  })),
);
const NetworkSettings = lazy(() =>
  import("@/routes/settings/network-settings").then((m) => ({
    default: m.NetworkSettings,
  })),
);
const ProfileSettings = lazy(() =>
  import("@/routes/settings/profile-settings").then((m) => ({
    default: m.ProfileSettings,
  })),
);
const RegionSettings = lazy(() =>
  import("@/routes/settings/region-settings").then((m) => ({
    default: m.RegionSettings,
  })),
);
const SecuritySettings = lazy(() =>
  import("@/routes/settings/security-settings").then((m) => ({
    default: m.SecuritySettings,
  })),
);
const SelfHealSettings = lazy(() =>
  import("@/routes/settings/self-heal-settings").then((m) => ({
    default: m.SelfHealSettings,
  })),
);
const SwarmSettings = lazy(() =>
  import("@/routes/settings/swarm-settings").then((m) => ({
    default: m.SwarmSettings,
  })),
);
const VisionSettings = lazy(() =>
  import("@/routes/settings/vision-settings").then((m) => ({
    default: m.VisionSettings,
  })),
);

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
                <Route path="/extensions" element={<ExtensionsRoute />} />
                <Route path="/peripherals" element={<PeripheralsRoute />} />
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
                  <Route
                    path="region"
                    element={
                      <ProfileGate allow={["drone", "ground_station"]}>
                        <RegionSettings />
                      </ProfileGate>
                    }
                  />
                  <Route
                    path="battery"
                    element={
                      <ProfileGate allow={["drone"]}>
                        <BatterySettings />
                      </ProfileGate>
                    }
                  />
                  <Route path="network" element={<NetworkSettings />} />
                  <Route path="cellular" element={<CellularSettings />} />
                  <Route path="mac-pin" element={<MacPinSettings />} />
                  <Route path="cloud" element={<CloudSettings />} />
                  <Route path="self-heal" element={<SelfHealSettings />} />
                  <Route
                    path="mavlink"
                    element={
                      <ProfileGate allow={["drone"]}>
                        <MavlinkSettings />
                      </ProfileGate>
                    }
                  />
                  <Route path="security" element={<SecuritySettings />} />
                  <Route
                    path="vision"
                    element={
                      <ProfileGate allow={["drone"]}>
                        <VisionSettings />
                      </ProfileGate>
                    }
                  />
                  <Route
                    path="camera"
                    element={
                      <ProfileGate allow={["drone"]}>
                        <CameraSettings />
                      </ProfileGate>
                    }
                  />
                  <Route
                    path="swarm"
                    element={
                      <ProfileGate allow={["drone"]}>
                        <SwarmSettings />
                      </ProfileGate>
                    }
                  />
                  <Route path="discovery" element={<DiscoverySettings />} />
                  <Route
                    path="display"
                    element={
                      <ProfileGate allow={["ground_station"]}>
                        <DisplaySettings />
                      </ProfileGate>
                    }
                  />
                  <Route path="advanced" element={<AdvancedSettings />} />
                </Route>
                <Route path="*" element={<NotFoundRoute />} />
              </Route>
              </Routes>
            </Suspense>
          </DashboardAccessGate>
        </ErrorBoundary>
      </TooltipProvider>
    </ThemeProvider>
  );
}
