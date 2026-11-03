// The screen registry — the single source of the cockpit's screens. The shell
// resolves the active screen from here; the menu lists the `tab` screens in
// TAB_ORDER. Later stages replace a screen's `render` (or add `detail` screens
// opened via `open-detail`) without touching the shell or the navigator.
//
// Feed is wired now (video + HUD); Link/Mesh/Pair/Uplink/System read their live
// agent surfaces; Node is the compute node's home (World Engine jobs, products
// and perception offload). Each profile opens on its own home tab. Settings is the full config tree: its tab renders the curated
// root, and each drill level (`settings:<path>`) resolves to a Settings screen
// bound to that path — so the shell's detail stack + hardware "back" drive the
// drill without a static screen per config group.

import {
  Gauge,
  Radio,
  Server,
  Network,
  Link2,
  SlidersHorizontal,
  Cpu,
  Settings,
  Blocks,
  Layers,
  ScrollText,
  Usb,
  Wrench,
  Terminal,
  Video,
  Antenna,
  RadioTower,
  Satellite,
  Activity,
  Gamepad2,
  ScanLine,
} from "lucide-react";

import { FeedScreen } from "@/components/screens/feed-screen";
import {
  ExtensionsScreen,
  LogsScreen,
  PeripheralsScreen,
  LinkScreen,
  MeshScreen,
  NodeScreen,
  MspScreen,
  ParamsScreen,
  PairScreen,
  SettingsScreen,
  SourcesScreen,
  SystemScreen,
  UplinkScreen,
  VideoScreen,
  TransmitScreen,
  LoraScreen,
  GnssScreen,
  SensorsScreen,
  IoScreen,
  CaptureScreen,
} from "@/nav/lazy-screens";
import { DEFAULT_TAB_ID, homeTabId, tabVisibleFor, type FcFamily, type ScreenSpec } from "@/nav/navigator";
import type { GroundRole } from "@/shared/api/types";
import type { AgentProfile } from "@/shared/use-profile";

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
    id: "node",
    title: "Node",
    icon: Server,
    kind: "tab",
    profiles: ["workstation", "compute"],
    render: () => <NodeScreen />,
  },
  {
    // The visor's home (HOME_BY_PROFILE.visor): the world-engine capture session.
    id: "capture",
    title: "Capture",
    icon: ScanLine,
    kind: "tab",
    profiles: ["visor"],
    render: () => <CaptureScreen />,
  },
  {
    id: "link",
    title: "Link",
    icon: Radio,
    kind: "tab",
    render: () => <LinkScreen />,
  },
  {
    id: "video",
    title: "Video",
    icon: Video,
    kind: "tab",
    profiles: ["drone", "visor"],
    render: () => <VideoScreen />,
  },
  {
    id: "transmit",
    title: "Transmit",
    icon: Antenna,
    kind: "tab",
    profiles: ["drone"],
    render: () => <TransmitScreen />,
  },
  {
    id: "lora",
    title: "LoRa",
    icon: RadioTower,
    kind: "tab",
    profiles: ["drone", "ground_station"],
    render: () => <LoraScreen />,
  },
  {
    id: "gnss",
    title: "GNSS",
    icon: Satellite,
    kind: "tab",
    profiles: ["ground_station", "visor"],
    render: () => <GnssScreen />,
  },
  {
    id: "sensors",
    title: "Sensors",
    icon: Activity,
    kind: "tab",
    profiles: ["drone", "visor"],
    render: () => <SensorsScreen />,
  },
  {
    id: "io",
    title: "IO",
    icon: Gamepad2,
    kind: "tab",
    profiles: ["ground_station"],
    render: () => <IoScreen />,
  },
  {
    id: "mesh",
    title: "Mesh",
    icon: Network,
    kind: "tab",
    profiles: ["ground_station"],
    // A direct ground station is not in a mesh.
    roles: ["relay", "receiver"],
    render: () => <MeshScreen />,
  },
  {
    id: "sources",
    title: "Sources",
    icon: Layers,
    kind: "tab",
    profiles: ["ground_station"],
    // Only a receiver combines fragments from relays.
    roles: ["receiver"],
    render: () => <SourcesScreen />,
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
    profiles: ["ground_station"],
    render: () => <UplinkScreen />,
  },
  {
    id: "system",
    title: "System",
    icon: Cpu,
    kind: "tab",
    render: (ctx) => <SystemScreen dispatch={ctx.dispatch} />,
  },
  {
    id: "peripherals",
    title: "Peripherals",
    icon: Usb,
    // Opened from the System screen with `open-detail`.
    kind: "detail",
    render: () => <PeripheralsScreen />,
  },
  {
    id: "logs",
    title: "Logs",
    icon: ScrollText,
    kind: "detail",
    render: () => <LogsScreen />,
  },
  {
    id: "extensions",
    title: "Extensions",
    icon: Blocks,
    kind: "tab",
    // Lists every running extension and hosts its tabs and panels; a node with
    // none shows a plain empty state.
    render: () => <ExtensionsScreen />,
  },
  {
    id: "params",
    title: "Params",
    icon: Wrench,
    kind: "tab",
    profiles: ["drone", "visor"],
    fcFamily: "mavlink",
    render: () => <ParamsScreen />,
  },
  {
    id: "msp",
    title: "MSP",
    icon: Terminal,
    kind: "tab",
    profiles: ["drone", "visor"],
    fcFamily: "msp",
    render: () => <MspScreen />,
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

/** The top-level tab screens, in menu order, for the agent profile, on a
 *  ground station its role, and the FC family once identified (see
 *  `tabVisibleFor`). */
export function tabScreens(
  profile?: AgentProfile | null,
  role?: GroundRole | null,
  fcFamily?: FcFamily | null,
): ScreenSpec[] {
  return SCREENS.filter((s) => s.kind === "tab" && tabVisibleFor(s, profile, role, fcFamily));
}

/** The home tab for `profile`, or null while the profile is unknown. */
export function homeTabFor(profile: AgentProfile | null | undefined): string | null {
  return homeTabId(profile, (id) => REGISTRY.has(id));
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
