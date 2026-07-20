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
  /** When true, the screen paints edge-to-edge (its own L0/L2 layers) and the
   *  shell floats its chrome translucently on top — the Feed's video view. When
   *  false (the default), the screen renders in a framed content region with
   *  solid chrome. */
  fullBleed?: boolean;
  render: (ctx: ScreenContext) => ReactNode;
}

/** The default tab the navigator opens on (the Feed, the pilot's flying view).
 *  Restored from persistence when a valid id was saved. */
export const DEFAULT_TAB_ID = "feed";

/** The ordered tab ids, left-to-right / top-to-bottom in the menu. The store
 *  uses this order to move the button/gamepad focus ring across the menu; the
 *  registry builds each screen against the same id. Detail screens are not
 *  listed here. */
export const TAB_ORDER: readonly string[] = [
  "feed",
  "link",
  "mesh",
  "pair",
  "uplink",
  "system",
  "settings",
];
