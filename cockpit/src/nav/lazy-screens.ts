import { lazy } from "react";

// The Feed ships in the main bundle; every other screen is its own chunk,
// loaded the first time it is opened, so the flying view starts fast on a
// small panel. The shell wraps the screen body in a Suspense boundary.
export const LinkScreen = lazy(() => import("@/components/screens/link-screen").then((m) => ({ default: m.LinkScreen })));
export const MeshScreen = lazy(() => import("@/components/screens/mesh-screen").then((m) => ({ default: m.MeshScreen })));
export const PairScreen = lazy(() => import("@/components/screens/pair-screen").then((m) => ({ default: m.PairScreen })));
export const ExtensionsScreen = lazy(() =>
  import("@/components/screens/extensions-screen").then((m) => ({ default: m.ExtensionsScreen })),
);
export const SettingsScreen = lazy(() =>
  import("@/components/screens/settings-screen").then((m) => ({ default: m.SettingsScreen })),
);
export const SystemScreen = lazy(() => import("@/components/screens/system-screen").then((m) => ({ default: m.SystemScreen })));
export const UplinkScreen = lazy(() => import("@/components/screens/uplink-screen").then((m) => ({ default: m.UplinkScreen })));
