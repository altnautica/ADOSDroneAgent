// The button a controller hint names, per input source. The gamepad uses the
// standard mapping (A select, B back, Start menu, the d-pad to move); the
// panel's own buttons are named by their printed role.

import type { NavCommand } from "@/nav/navigator";
import type { InputSource } from "@/stores/input-store";

const GAMEPAD: Record<NavCommand, string> = {
  activate: "A",
  back: "B",
  "quick-menu": "Start",
  prev: "D-pad",
  next: "D-pad",
  "cycle-tab": "D-pad",
};

const PANEL: Record<NavCommand, string> = {
  activate: "OK",
  back: "Back",
  "quick-menu": "Hold OK",
  prev: "▲",
  next: "▼",
  "cycle-tab": "Cycle",
};

/** The glyph for `cmd` on `source`, or null for a source with no buttons. */
export function glyphFor(source: InputSource, cmd: NavCommand): string | null {
  if (source === "gamepad") return GAMEPAD[cmd];
  if (source === "panel") return PANEL[cmd];
  return null;
}
