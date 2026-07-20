// The screen registry — the single source of the cockpit's screens. The shell
// resolves the active screen from here; the menu lists the `tab` screens in
// TAB_ORDER. Later stages replace a screen's `render` (or add `detail` screens
// opened via `open-detail`) without touching the shell or the navigator.
//
// Feed is wired now (video + HUD); Link/Mesh/Pair/Uplink/System read their live
// agent surfaces. Settings stays a placeholder body that names the surfaces it
// will read, so the shell is fully navigable and that screen has a real slot to
// grow into (the settings-tree stage replaces its `render`).

import {
  Gauge,
  Radio,
  Network,
  Link2,
  SlidersHorizontal,
  Cpu,
  Settings,
} from "lucide-react";

import { FeedScreen } from "@/components/screens/feed-screen";
import { LinkScreen } from "@/components/screens/link-screen";
import { MeshScreen } from "@/components/screens/mesh-screen";
import { PairScreen } from "@/components/screens/pair-screen";
import { PlaceholderScreen } from "@/components/screens/placeholder-screen";
import { SystemScreen } from "@/components/screens/system-screen";
import { UplinkScreen } from "@/components/screens/uplink-screen";
import { DEFAULT_TAB_ID, type ScreenSpec } from "@/nav/navigator";

const SCREENS: ScreenSpec[] = [
  {
    id: "feed",
    title: "Feed",
    icon: Gauge,
    kind: "tab",
    fullBleed: true,
    render: () => <FeedScreen />,
  },
  {
    id: "link",
    title: "Link",
    icon: Radio,
    kind: "tab",
    render: () => <LinkScreen />,
  },
  {
    id: "mesh",
    title: "Mesh",
    icon: Network,
    kind: "tab",
    render: () => <MeshScreen />,
  },
  {
    id: "pair",
    title: "Pair",
    icon: Link2,
    kind: "tab",
    render: () => <PairScreen />,
  },
  {
    id: "uplink",
    title: "Uplink",
    icon: SlidersHorizontal,
    kind: "tab",
    render: () => <UplinkScreen />,
  },
  {
    id: "system",
    title: "System",
    icon: Cpu,
    kind: "tab",
    render: () => <SystemScreen />,
  },
  {
    id: "settings",
    title: "Settings",
    icon: Settings,
    kind: "tab",
    render: () => (
      <PlaceholderScreen
        title="Settings"
        icon={Settings}
        summary="Every agent parameter, settable in the field: a 48px drill-down tree over the whole config with type-inferred editors and a reboot-required banner."
        reads={["GET /api/config", "PUT /api/config"]}
      />
    ),
  },
];

const REGISTRY = new Map<string, ScreenSpec>(SCREENS.map((s) => [s.id, s]));

/** Every registered screen, in declaration order. */
export function allScreens(): ScreenSpec[] {
  return SCREENS;
}

/** The top-level tab screens, in menu order. */
export function tabScreens(): ScreenSpec[] {
  return SCREENS.filter((s) => s.kind === "tab");
}

/** Resolve a screen by id, falling back to the default tab if the id is
 *  unknown (mirrors the TFT navigator's fall-through to a stable first page). */
export function getScreen(id: string): ScreenSpec {
  return REGISTRY.get(id) ?? REGISTRY.get(DEFAULT_TAB_ID) ?? SCREENS[0];
}
