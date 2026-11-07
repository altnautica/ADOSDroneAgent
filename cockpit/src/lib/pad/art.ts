// The controller drawings from the cockpit spec: `ds4-art.json` /
// `ds4-hotspots.json` for a DualShock 4 and `gamepad-generic-*` for any other
// standard-mapping pad, drawn from their `d` strings (identical on every
// surface), plus the hit areas and leader-line anchors. Also the pad-kind
// detection and the colours the drawing uses (`tokens.json` controller.*).

import ds4Art from "@/shared/cockpit-spec/ds4-art.json";
import ds4Hotspots from "@/shared/cockpit-spec/ds4-hotspots.json";
import genericArt from "@/shared/cockpit-spec/gamepad-generic-art.json";
import genericHotspots from "@/shared/cockpit-spec/gamepad-generic-hotspots.json";
import tokens from "@/shared/cockpit-spec/tokens.json";

import type { PadKind } from "@/lib/pad/bindings";
import type { ControlId } from "@/lib/pad/ds4-map";

export interface ArtPath {
  id: string;
  d: string;
  role: "body" | "control" | "detail";
  control?: ControlId;
}

export interface PadArt {
  viewBox: [number, number, number, number];
  strokeWidth: number;
  paths: ArtPath[];
}

export type HotspotGeom =
  | { shape: "circle"; geom: { cx: number; cy: number; r: number } }
  | { shape: "rect"; geom: { x: number; y: number; w: number; h: number; rx?: number } }
  | { shape: "path"; geom: { d: string } };

export type Hotspot = HotspotGeom & {
  label_anchor: [number, number];
  label_side: "left" | "right" | "top" | "bottom";
};

export interface PadDrawing {
  art: PadArt;
  hotspots: Partial<Record<ControlId, Hotspot>>;
}

function drawing(art: typeof ds4Art | typeof genericArt, hs: typeof ds4Hotspots | typeof genericHotspots): PadDrawing {
  return {
    art: {
      viewBox: art.viewBox as [number, number, number, number],
      strokeWidth: art.stroke_width,
      paths: art.paths as ArtPath[],
    },
    hotspots: hs.hotspots as Partial<Record<ControlId, Hotspot>>,
  };
}

export const DRAWINGS: Readonly<Record<PadKind, PadDrawing>> = {
  ds4: drawing(ds4Art, ds4Hotspots),
  generic: drawing(genericArt, genericHotspots),
};

export const PAD_COLORS = tokens.controller as {
  pressed: string;
  conflict: string;
  deadzone_ring: string;
  art_stroke: string;
  art_fill: string;
};

/** A DualShock 4 by its USB vendor (Sony 054c) or its reported name. */
export function padKindOf(id: string): PadKind {
  return /054c|wireless controller|dualshock/i.test(id) ? "ds4" : "generic";
}

/** `Wireless Controller (STANDARD GAMEPAD Vendor: 054c Product: 09cc)` →
 *  `Wireless Controller`; Firefox's `054c-09cc-Wireless Controller` →
 *  `Wireless Controller`. */
export function padDisplayName(id: string): string {
  const chrome = id.replace(/\s*\((?:STANDARD GAMEPAD\s*)?Vendor:.*\)\s*$/i, "").trim();
  const firefox = chrome.replace(/^[0-9a-f]{4}-[0-9a-f]{4}-/i, "").trim();
  return firefox || id;
}
