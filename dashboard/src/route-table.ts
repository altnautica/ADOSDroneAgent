import { lazy, type ComponentType } from "react";

import type { AllowedProfile } from "@/components/profile-gate";
import type { GroundRole } from "@/lib/types";

import { HomeRoute } from "@/routes/home";

import table from "./route-table.json";

/** One dashboard page. `profiles`/`roles` null means every profile/role. */
export interface RouteEntry {
  key: string;
  path: string;
  label: string;
  profiles: AllowedProfile[] | null;
  roles: GroundRole[] | null;
}

export const ROUTES = table as readonly RouteEntry[];

/** Every page key. Kept literal so a missing component is a type error. */
export type RouteKey =
  | "home"
  | "pairing"
  | "receive"
  | "mesh"
  | "sources"
  | "extensions"
  | "peripherals"
  | "logs"
  | "diagnostics"
  | "telemetry"
  | "video"
  | "transmit"
  | "io"
  | "gnss"
  | "lora"
  | "capture"
  | "sensors"
  | "settings-profile"
  | "settings-region"
  | "settings-battery"
  | "settings-network"
  | "settings-cellular"
  | "settings-mac-pin"
  | "settings-cloud"
  | "settings-self-heal"
  | "settings-mavlink"
  | "settings-security"
  | "settings-vision"
  | "settings-camera"
  | "settings-capture"
  | "settings-sensors"
  | "settings-swarm"
  | "settings-discovery"
  | "settings-display"
  | "settings-advanced";

export const SETTINGS_PREFIX = "/settings/";

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
const GnssRoute = lazy(() =>
  import("@/routes/ground-pages").then((m) => ({ default: m.GnssRoute })),
);
const LoraRoute = lazy(() =>
  import("@/routes/ground-pages").then((m) => ({ default: m.LoraRoute })),
);
const CaptureRoute = lazy(() =>
  import("@/routes/visor-pages").then((m) => ({ default: m.CaptureRoute })),
);
const SensorsRoute = lazy(() =>
  import("@/routes/visor-pages").then((m) => ({ default: m.SensorsRoute })),
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
const CaptureSettings = lazy(() =>
  import("@/routes/settings/capture-settings").then((m) => ({
    default: m.CaptureSettings,
  })),
);
const SensorsSettings = lazy(() =>
  import("@/routes/settings/sensors-settings").then((m) => ({
    default: m.SensorsSettings,
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

/** The page component for every key in `route-table.json`. */
export const ROUTE_COMPONENTS: Record<RouteKey, ComponentType> = {
  "home": HomeRoute,
  "pairing": PairingRoute,
  "receive": ReceiveRoute,
  "mesh": MeshRoute,
  "sources": SourcesRoute,
  "extensions": ExtensionsRoute,
  "peripherals": PeripheralsRoute,
  "logs": LogsRoute,
  "diagnostics": DiagnosticsRoute,
  "telemetry": TelemetryRoute,
  "video": VideoRoute,
  "transmit": TransmitRoute,
  "io": IoRoute,
  "gnss": GnssRoute,
  "lora": LoraRoute,
  "capture": CaptureRoute,
  "sensors": SensorsRoute,
  "settings-profile": ProfileSettings,
  "settings-region": RegionSettings,
  "settings-battery": BatterySettings,
  "settings-network": NetworkSettings,
  "settings-cellular": CellularSettings,
  "settings-mac-pin": MacPinSettings,
  "settings-cloud": CloudSettings,
  "settings-self-heal": SelfHealSettings,
  "settings-mavlink": MavlinkSettings,
  "settings-security": SecuritySettings,
  "settings-vision": VisionSettings,
  "settings-camera": CameraSettings,
  "settings-capture": CaptureSettings,
  "settings-sensors": SensorsSettings,
  "settings-swarm": SwarmSettings,
  "settings-discovery": DiscoverySettings,
  "settings-display": DisplaySettings,
  "settings-advanced": AdvancedSettings,
};

