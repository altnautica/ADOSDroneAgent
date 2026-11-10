// The dashboard video panel's layer lease: the Route-level base layers plus
// the depth and surfaces layers when toggled on (`layers.json` `lease: true`,
// off by default).

import layersJson from "@/shared/cockpit-spec/layers.json";

/** The primary camera's served id. */
export const CAMERA_ID = "main";
const BASE_LAYERS = ["ar_route", "ar_motion", "obstacles"] as const;

/** The leased layers toggled on their own (`layers.json` `lease: true`), with
 *  their spec label and default. */
export const LEASED_LAYERS: readonly { id: string; label: string; default: boolean }[] = layersJson.layers
  .filter((l) => l.lease)
  .map((l) => ({ id: l.id, label: l.label, default: l.default }));

/** The leased layers on by default. */
export function defaultLeasedLayers(): Record<string, boolean> {
  return Object.fromEntries(LEASED_LAYERS.map((l) => [l.id, l.default]));
}

/** The view query for the base layers plus the toggled-on leased ones. */
export function layersViewQuery(toggles: Readonly<Record<string, boolean>>): string {
  const on = LEASED_LAYERS.filter((l) => toggles[l.id]).map((l) => l.id);
  return `${CAMERA_ID}:${[...BASE_LAYERS, ...on].join(",")}`;
}
