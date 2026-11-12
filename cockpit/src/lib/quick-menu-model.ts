// The Menu's entries in the cockpit spec's order (`layout.json` `menu.order`):
// the top-bar items the bar has moved here (only those actually hidden),
// View, Cameras, Layers, Pre-flight, Flight modes, Controls, Extension
// panels, Set up aircraft (only while setup is needed or offered, and the
// vehicle is known disarmed on the ground), Settings and Exit
// (to the node dashboard; the kiosk has none). Off the flight screen the
// flight entries have nothing to act on and are left out.

import layoutJson from "@/shared/cockpit-spec/layout.json";

export type MenuEntryId =
  | "overflow"
  | "view"
  | "cameras"
  | "layers"
  | "preflight"
  | "modes"
  | "controls"
  | "extension_panels"
  | "setup"
  | "settings"
  | "exit";

export const MENU_ORDER: readonly MenuEntryId[] = layoutJson.menu.order as MenuEntryId[];

/** Entries that act on the flight screen. */
const FLIGHT_ENTRY: Partial<Record<MenuEntryId, true>> = { overflow: true, view: true, cameras: true, layers: true, preflight: true, modes: true };

export interface MenuInputs {
  /** The Menu is open over the flight screen. */
  onFeed: boolean;
  /** How many top-bar items the bar has moved into the Menu. */
  movedCount: number;
  /** The video legs the node offers. */
  cameraCount: number;
  /** Running extensions' cockpit panels. */
  panelCount: number;
  /** The pilot reports the aircraft needs setting up. */
  setupNeeded: boolean;
  /** The one-time setup offer stands (the shell banner shows it). */
  setupOffered: boolean;
  /** The vehicle is known disarmed on the ground (an unknown arm state counts
   *  as airborne); Set up aircraft is offered only then. */
  knownOnGround: boolean;
  kiosk: boolean;
}

export function menuEntries(i: MenuInputs): MenuEntryId[] {
  return MENU_ORDER.filter((id) => {
    if (!i.onFeed && FLIGHT_ENTRY[id]) return false;
    switch (id) {
      case "overflow":
        return i.movedCount > 0;
      case "cameras":
        return i.cameraCount > 1;
      case "extension_panels":
        return i.panelCount > 0;
      case "setup":
        return i.knownOnGround && (i.setupNeeded || i.setupOffered);
      case "exit":
        return !i.kiosk;
      default:
        return true;
    }
  });
}
