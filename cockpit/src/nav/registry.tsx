// The screen registry — the single source of the cockpit's screens. The shell
// resolves the active screen from here; the menu lists the `tab` screens in
// TAB_ORDER. Later stages replace a screen's `render` (or add `detail` screens
// opened via `open-detail`) without touching the shell or the navigator.
//
// Feed is wired now (video + HUD); Link/Mesh/Pair/Uplink/System read their live
// agent surfaces. Settings is the full config tree: its tab renders the curated
// root, and each drill level (`settings:<path>`) resolves to a Settings screen
// bound to that path — so the shell's detail stack + hardware "back" drive the
// drill without a static screen per config group.

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
import { SettingsScreen } from "@/components/screens/settings-screen";
import { SystemScreen } from "@/components/screens/system-screen";
import { UplinkScreen } from "@/components/screens/uplink-screen";
import { DEFAULT_TAB_ID, type ScreenSpec } from "@/nav/navigator";

/** Prefix on the detail-stack id for a Settings drill level. Everything after
 *  it is the drill path the Settings screen renders (`~`/`~group`/`#`/`#path`/
 *  `@path`). */
const SETTINGS_DETAIL_PREFIX = "settings:";

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
    render: (ctx) => <SettingsScreen path="~" dispatch={ctx.dispatch} />,
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
 *  unknown (mirrors the TFT navigator's fall-through to a stable first page).
 *  A `settings:<path>` id is a dynamic Settings drill level: it resolves to the
 *  Settings screen bound to that drill path (registered nowhere, so it never
 *  appears in the menu — a detail screen pushed onto the stack). */
export function getScreen(id: string): ScreenSpec {
  if (id.startsWith(SETTINGS_DETAIL_PREFIX)) {
    const path = id.slice(SETTINGS_DETAIL_PREFIX.length);
    return {
      id,
      title: "Settings",
      icon: Settings,
      kind: "detail",
      render: (ctx) => <SettingsScreen path={path} dispatch={ctx.dispatch} />,
    };
  }
  return REGISTRY.get(id) ?? REGISTRY.get(DEFAULT_TAB_ID) ?? SCREENS[0];
}
