import { lazy } from "react";

// The Feed ships in the main bundle; every other screen is its own chunk,
// loaded the first time it is opened, so the flying view starts fast on a
// small panel. The shell wraps the screen body in a Suspense boundary.
export const LinkScreen = lazy(() => import("@/components/screens/link-screen").then((m) => ({ default: m.LinkScreen })));
export const MeshScreen = lazy(() => import("@/components/screens/mesh-screen").then((m) => ({ default: m.MeshScreen })));
export const PairScreen = lazy(() => import("@/components/screens/pair-screen").then((m) => ({ default: m.PairScreen })));
export const SourcesScreen = lazy(() => import("@/components/screens/sources-screen").then((m) => ({ default: m.SourcesScreen })));
export const NodeScreen = lazy(() => import("@/components/screens/node-screen").then((m) => ({ default: m.NodeScreen })));
export const ExtensionsScreen = lazy(() =>
  import("@/components/screens/extensions-screen").then((m) => ({ default: m.ExtensionsScreen })),
);
export const SettingsScreen = lazy(() =>
  import("@/components/screens/settings-screen").then((m) => ({ default: m.SettingsScreen })),
);
export const SystemScreen = lazy(() => import("@/components/screens/system-screen").then((m) => ({ default: m.SystemScreen })));
export const UplinkScreen = lazy(() => import("@/components/screens/uplink-screen").then((m) => ({ default: m.UplinkScreen })));
export const LogsScreen = lazy(() => import("@/components/screens/logs-screen").then((m) => ({ default: m.LogsScreen })));
export const PeripheralsScreen = lazy(() =>
  import("@/components/screens/peripherals-screen").then((m) => ({ default: m.PeripheralsScreen })),
);
export const ParamsScreen = lazy(() => import("@/components/screens/params-screen").then((m) => ({ default: m.ParamsScreen })));
export const MspScreen = lazy(() => import("@/components/screens/msp-screen").then((m) => ({ default: m.MspScreen })));
export const VideoScreen = lazy(() => import("@/components/screens/video-screen").then((m) => ({ default: m.VideoScreen })));
export const TransmitScreen = lazy(() =>
  import("@/components/screens/transmit-screen").then((m) => ({ default: m.TransmitScreen })),
);
export const IoScreen = lazy(() => import("@/components/screens/io-screen").then((m) => ({ default: m.IoScreen })));
export const GnssScreen = lazy(() => import("@/components/screens/gnss-screen").then((m) => ({ default: m.GnssScreen })));
export const LoraScreen = lazy(() => import("@/components/screens/lora-screen").then((m) => ({ default: m.LoraScreen })));
export const CaptureScreen = lazy(() =>
  import("@/components/screens/capture-screen").then((m) => ({ default: m.CaptureScreen })),
);
export const SensorsScreen = lazy(() =>
  import("@/components/screens/sensors-screen").then((m) => ({ default: m.SensorsScreen })),
);
