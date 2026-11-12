// The screen registry — the single source of the cockpit's screens. The shell
// resolves the active screen from here; the menu lists the `tab` screens in
// registry order (TAB_ORDER, derived below). Later stages replace a screen's `render` (or add `detail` screens
// opened via `open-detail`) without touching the shell or the navigator.
//
// Feed is wired now (video + HUD); Link/Mesh/Pair/Uplink/System read their live
// agent surfaces; Node is the compute node's home (World Engine jobs, products
// and perception offload). Each profile opens on its own home tab. Settings is the full config tree: its tab renders the curated
// root, and each drill level (`settings:<path>`) resolves to a Settings screen
// bound to that path — so the shell's detail stack + hardware "back" drive the
// drill without a static screen per config group.

import { Settings } from "lucide-react";

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
  AircraftSetupScreen,
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
import { SCREEN_TABLE } from "@/nav/screen-table";
import i18n from "@/shared/i18n";
import type { GroundRole } from "@/shared/api/types";
import type { AgentProfile } from "@/shared/use-profile";

/** Prefix on the detail-stack id for a Settings drill level. Everything after
 *  it is the drill path the Settings screen renders (`~`/`~group`/`#`/`#path`/
 *  `@path`). */
const SETTINGS_DETAIL_PREFIX = "settings:";

const RENDERS: Record<string, ScreenSpec["render"]> = {
  feed: () => <FeedScreen />,
  node: () => <NodeScreen />,
  capture: () => <CaptureScreen />,
  link: () => <LinkScreen />,
  video: () => <VideoScreen />,
  transmit: () => <TransmitScreen />,
  lora: () => <LoraScreen />,
  gnss: () => <GnssScreen />,
  sensors: () => <SensorsScreen />,
  io: () => <IoScreen />,
  mesh: () => <MeshScreen />,
  sources: () => <SourcesScreen />,
  pair: () => <PairScreen />,
  uplink: () => <UplinkScreen />,
  system: (ctx) => <SystemScreen dispatch={ctx.dispatch} />,
  peripherals: () => <PeripheralsScreen />,
  "aircraft-setup": () => <AircraftSetupScreen />,
  logs: () => <LogsScreen />,
  extensions: () => <ExtensionsScreen />,
  params: () => <ParamsScreen />,
  msp: () => <MspScreen />,
  settings: (ctx) => <SettingsScreen path="~" dispatch={ctx.dispatch} />,
};

const SCREENS: ScreenSpec[] = SCREEN_TABLE.map((meta) => ({
  ...meta,
  // Read at render time, so the label follows the active language.
  get title() {
    return i18n.t(meta.labelKey);
  },
  render: RENDERS[meta.id],
}));

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
      get title() {
        return i18n.t("nav.settings");
      },
      icon: Settings,
      kind: "detail",
      render: (ctx) => <SettingsScreen path={path} dispatch={ctx.dispatch} />,
    };
  }
  return REGISTRY.get(id) ?? REGISTRY.get(DEFAULT_TAB_ID) ?? SCREENS[0];
}
