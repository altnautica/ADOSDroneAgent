// The sidebar's page list comes from the route table: which profile and role
// sees each page is decided there, once, so the sidebar and the route gates
// cannot drift apart. This file adds only each page's label and icon.

import {
  Activity,
  Antenna,
  Aperture,
  Cpu,
  Gamepad2,
  Home,
  Layers,
  Link2,
  Network,
  Plug,
  Radio,
  RadioTower,
  Satellite,
  ScrollText,
  Settings as SettingsIcon,
  Video,
  Wrench,
  type LucideIcon,
} from "lucide-react";
import type { ParseKeys } from "i18next";

import { gateDecision } from "@/lib/node-identity";
import { ROUTES, SETTINGS_PREFIX, type RouteEntry, type RouteKey } from "@/route-table";
import type { GroundRole, Profile } from "@/shared/api/types";

export interface NavItem {
  to: string;
  labelKey: ParseKeys;
  icon: LucideIcon;
}

const NAV: Partial<Record<RouteKey, { labelKey: ParseKeys; icon: LucideIcon }>> = {
  home: { labelKey: "nav.home", icon: Home },
  pairing: { labelKey: "nav.pairing", icon: Link2 },
  receive: { labelKey: "nav.receive", icon: Antenna },
  mesh: { labelKey: "nav.mesh", icon: Network },
  sources: { labelKey: "nav.sources", icon: Layers },
  extensions: { labelKey: "nav.extensions", icon: Plug },
  peripherals: { labelKey: "nav.peripherals", icon: Cpu },
  logs: { labelKey: "nav.logs", icon: ScrollText },
  diagnostics: { labelKey: "nav.diagnostics", icon: Wrench },
  telemetry: { labelKey: "nav.telemetry", icon: Radio },
  video: { labelKey: "nav.video", icon: Video },
  transmit: { labelKey: "nav.transmit", icon: Antenna },
  io: { labelKey: "nav.io", icon: Gamepad2 },
  gnss: { labelKey: "nav.gnss", icon: Satellite },
  lora: { labelKey: "nav.lora", icon: RadioTower },
  capture: { labelKey: "nav.capture", icon: Aperture },
  sensors: { labelKey: "nav.sensors", icon: Activity },
};

const SETTINGS_ITEM: NavItem = { to: "/settings", labelKey: "nav.settings", icon: SettingsIcon };

function toItem(r: RouteEntry): NavItem[] {
  const nav = NAV[r.key as RouteKey];
  // Home lives at "/" (the table's "/home" redirects there).
  return nav ? [{ to: r.key === "home" ? "/" : r.path, ...nav }] : [];
}

/** The sidebar groups for a node: Home; the pages its profile and role allow
 *  (none while the profile is unresolved); the pages every node has; Settings. */
export function navGroups(profile: Profile, role: GroundRole): {
  top: NavItem[];
  profileItems: NavItem[];
  bottom: NavItem[];
} {
  const pages = ROUTES.filter((r) => !r.path.startsWith(SETTINGS_PREFIX));
  const known = profile !== "auto" && profile !== "unknown";
  return {
    top: pages.filter((r) => r.key === "home").flatMap(toItem),
    profileItems: known
      ? pages
          .filter((r) => r.profiles !== null && gateDecision(profile, role, r.profiles, r.roles ?? undefined) === "pass")
          .flatMap(toItem)
      : [],
    bottom: [...pages.filter((r) => r.profiles === null && r.key !== "home").flatMap(toItem), SETTINGS_ITEM],
  };
}
