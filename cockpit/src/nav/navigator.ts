// The cockpit navigator contract.
//
// This mirrors the on-board TFT navigator (crates/ados-display): a set of
// registered screens, an active tab, and a detail/modal stack. A screen row
// raises a `ScreenAction` (the web analog of the TFT `HitAction`); the store
// applies it (the analog of the TFT `PageNavigator` + `Dispatch`). Later
// stages register their screens against this contract without touching the
// shell.

import type { ReactNode } from "react";
import type { LucideIcon } from "lucide-react";

import type { GroundRole } from "@/shared/api/types";
import type { AgentProfile } from "@/shared/use-profile";

/** A screen-raised navigation intent — the web analog of the TFT `HitAction`.
 *  `go-tab` switches the active tab, `open-detail` pushes a detail screen onto
 *  the stack, `back` pops it, and `custom` is a screen-defined key the owning
 *  screen interprets (settings rows, list items, actions). */
export type ScreenAction =
  | { kind: "go-tab"; id: string }
  | { kind: "open-detail"; id: string }
  | { kind: "back" }
  | { kind: "custom"; key: string };

/** A `tab` screen is a top-level menu entry; a `detail` screen is pushed onto
 *  the detail stack via `open-detail` and never appears in the menu. */
export type ScreenKind = "tab" | "detail";

/** The folded input command set. The single dispatcher maps every input source
 *  (touch focus, physical buttons, gamepad) onto exactly these, so the menu and
 *  screens respond identically regardless of source. */
export type NavCommand =
  | "prev" // move the focus ring up / previous
  | "next" // move the focus ring down / next
  | "activate" // fire the focused target
  | "back" // pop a detail screen / dismiss the quick menu
  | "cycle-tab" // advance to the next tab
  | "quick-menu"; // toggle the quick menu

/** What a screen's `render` receives. Screens are React components, so they may
 *  use hooks directly for data; `dispatch` is how a row raises a ScreenAction. */
export interface ScreenContext {
  dispatch: (action: ScreenAction) => void;
}

/** A registered screen. `render` returns the screen body; the shell paints the
 *  chrome (status strip, menu, action bar) around it. */
export interface ScreenSpec {
  id: string;
  /** Short label shown in the menu (`tab` screens) or the breadcrumb. */
  title: string;
  /** Optional menu icon. */
  icon?: LucideIcon;
  kind: ScreenKind;
  /** When true, the screen may paint edge-to-edge (its own L0/L2 layers): the
   *  shell draws it full-bleed with no chrome while the cockpit is full screen
   *  (the kiosk, or the top bar's Full screen button), and otherwise frames it
   *  beside the menu rail without the status strip or action bar. When false
   *  (the default), the screen renders in a framed content region with solid
   *  chrome. */
  fullBleed?: boolean;
  /** The profiles the tab appears on (the mesh / uplink screens are
   *  receive-side concepts only a ground station has). Absent = shown on every
   *  profile. */
  profiles?: AgentProfile[];
  /** On a ground station, the roles the tab appears on (`direct`, `relay`,
   *  `receiver`). Absent = every role. Ignored on other profiles. */
  roles?: GroundRole[];
  /** The flight-controller protocol the tab serves (MAVLink parameters vs MSP
   *  configurator). Absent = any FC. Ignored while the FC is not identified. */
  fcFamily?: FcFamily;
  render: (ctx: ScreenContext) => ReactNode;
}

/** The fallback tab: the Feed, the pilot's flying view. A profile's own home
 *  tab (`homeTabFor`) takes over once the profile is known. */
export const DEFAULT_TAB_ID = "feed";

/** How the node's flight controller is configured: `mavlink` (ArduPilot, PX4)
 *  or `msp` (Betaflight, iNav). */
export type FcFamily = "mavlink" | "msp";

/** The FC family from the agent's firmware family and MSP variant, or null
 *  while it is not identified. */
export function fcFamilyOf(fcFirmware?: string | null, fcVariant?: string | null): FcFamily | null {
  const fw = (fcFirmware ?? "").toLowerCase();
  const variant = (fcVariant ?? "").toLowerCase();
  if (fw === "betaflight" || fw === "inav" || variant === "betaflight" || variant === "inav") return "msp";
  if (fw === "ardupilot" || fw === "px4") return "mavlink";
  return null;
}

/** Whether a screen's tab shows on `profile` and, on a ground station, on
 *  `role`. A tab with `profiles` shows only on those profiles (Mesh and Uplink
 *  are ground-station only); a tab with `roles` only on those ground-station
 *  roles (Mesh needs a relay or a receiver); a tab with `fcFamily` only for
 *  that kind of flight controller. Before the probe resolves, or on a profile
 *  it does not recognise, every tab shows and the menu settles once it is
 *  known; an unknown role or FC family hides no tab. */
export function tabVisibleFor(
  spec: Pick<ScreenSpec, "profiles" | "roles" | "fcFamily">,
  profile?: AgentProfile | null,
  role?: GroundRole | null,
  fcFamily?: FcFamily | null,
): boolean {
  if (!profile || profile === "unknown") return true;
  if (spec.profiles && !spec.profiles.includes(profile)) return false;
  if (spec.roles && profile === "ground_station" && role && !spec.roles.includes(role)) return false;
  if (spec.fcFamily && fcFamily && spec.fcFamily !== fcFamily) return false;
  return true;
}

/** Each profile's home tab. A ground station receives its drone's video, so
 *  it opens on the Feed like a drone; a visor opens on Capture; a workstation
 *  or compute node on its node screen. */
export const HOME_BY_PROFILE: Readonly<Record<Exclude<AgentProfile, "unknown">, string>> = {
  drone: "feed",
  ground_station: "feed",
  visor: "capture",
  workstation: "node",
  compute: "node",
};

/** The home tab id for `profile`, or null while the profile is unknown. A
 *  home the build does not register (`registered`) falls back to the Feed. */
export function homeTabId(
  profile: AgentProfile | null | undefined,
  registered: (id: string) => boolean,
): string | null {
  if (!profile || profile === "unknown") return null;
  const home = HOME_BY_PROFILE[profile];
  return registered(home) ? home : DEFAULT_TAB_ID;
}
