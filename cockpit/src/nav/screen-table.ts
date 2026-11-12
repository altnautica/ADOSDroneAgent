// The screen table: every cockpit screen's id, menu title, icon, kind and
// visibility, without its component. Kept apart from `registry.tsx` so the
// nav store can read the tab order without importing every screen (the
// screens import the store).

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

import type { ScreenSpec } from "@/nav/navigator";
import type en from "@/shared/locales/en.json";

/** A menu label: a key in the shared `nav.*` table, so the cockpit and the
 *  dashboard name each page the same way. */
export type NavLabelKey = `nav.${keyof (typeof en)["nav"]}`;

/** A screen's registration minus its component; `labelKey` resolves to the
 *  spec's `title` in the operator's language. */
export type ScreenMeta = Omit<ScreenSpec, "render" | "title"> & { labelKey: NavLabelKey };

export const SCREEN_TABLE: readonly ScreenMeta[] = [
  {
    id: "feed",
    labelKey: "nav.feed",
    icon: Gauge,
    kind: "tab",
    fullBleed: true,
  },
  {
    id: "node",
    labelKey: "nav.node",
    icon: Server,
    kind: "tab",
    profiles: ["workstation", "compute"],
  },
  {
    // The visor's home (HOME_BY_PROFILE.visor): the world-engine capture session.
    id: "capture",
    labelKey: "nav.capture",
    icon: ScanLine,
    kind: "tab",
    profiles: ["visor"],
  },
  {
    id: "link",
    labelKey: "nav.link",
    icon: Radio,
    kind: "tab",
  },
  {
    id: "video",
    labelKey: "nav.video",
    icon: Video,
    kind: "tab",
    profiles: ["drone", "visor"],
  },
  {
    id: "transmit",
    labelKey: "nav.transmit",
    icon: Antenna,
    kind: "tab",
    profiles: ["drone"],
  },
  {
    id: "lora",
    labelKey: "nav.lora",
    icon: RadioTower,
    kind: "tab",
    profiles: ["drone", "ground_station"],
  },
  {
    id: "gnss",
    labelKey: "nav.gnss",
    icon: Satellite,
    kind: "tab",
    profiles: ["ground_station", "visor"],
  },
  {
    id: "sensors",
    labelKey: "nav.sensors",
    icon: Activity,
    kind: "tab",
    profiles: ["drone", "visor"],
  },
  {
    id: "io",
    labelKey: "nav.io",
    icon: Gamepad2,
    kind: "tab",
    profiles: ["ground_station"],
  },
  {
    id: "mesh",
    labelKey: "nav.mesh",
    icon: Network,
    kind: "tab",
    profiles: ["ground_station"],
    // A direct ground station is not in a mesh.
    roles: ["relay", "receiver"],
  },
  {
    id: "sources",
    labelKey: "nav.sources",
    icon: Layers,
    kind: "tab",
    profiles: ["ground_station"],
    // Only a receiver combines fragments from relays.
    roles: ["receiver"],
  },
  {
    id: "pair",
    labelKey: "nav.pairing",
    icon: Link2,
    kind: "tab",
  },
  {
    id: "uplink",
    labelKey: "nav.uplink",
    icon: SlidersHorizontal,
    kind: "tab",
    profiles: ["ground_station"],
  },
  {
    id: "system",
    labelKey: "nav.system",
    icon: Cpu,
    kind: "tab",
  },
  {
    id: "peripherals",
    labelKey: "nav.peripherals",
    icon: Usb,
    // Opened from the System screen with `open-detail`.
    kind: "detail",
  },
  {
    id: "aircraft-setup",
    labelKey: "nav.aircraftSetup",
    icon: Wrench,
    // Opened from Settings and from the flight-state chip's "Set up aircraft".
    kind: "detail",
    profiles: ["drone", "ground_station"],
  },
  {
    id: "logs",
    labelKey: "nav.logs",
    icon: ScrollText,
    kind: "detail",
  },
  {
    id: "extensions",
    labelKey: "nav.extensions",
    icon: Blocks,
    kind: "tab",
    // Lists every running extension and hosts its tabs and panels; a node with
    // none shows a plain empty state.
  },
  {
    id: "params",
    labelKey: "nav.params",
    icon: Wrench,
    kind: "tab",
    profiles: ["drone", "visor"],
    fcFamily: "mavlink",
  },
  {
    id: "msp",
    labelKey: "nav.msp",
    icon: Terminal,
    kind: "tab",
    profiles: ["drone", "visor"],
    fcFamily: "msp",
  },
  {
    id: "settings",
    labelKey: "nav.settings",
    icon: Settings,
    kind: "tab",
  },
];

/** The ordered tab ids, in menu order: every registered `tab` screen. The nav
 *  store accepts exactly these in `goTab` and walks them with the focus ring;
 *  detail screens are not listed. Derived from the table so a new tab cannot
 *  be registered without becoming reachable. */
export const TAB_ORDER: readonly string[] = SCREEN_TABLE.filter((s) => s.kind === "tab").map((s) => s.id);
