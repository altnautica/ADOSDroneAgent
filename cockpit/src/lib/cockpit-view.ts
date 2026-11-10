// The cockpit view and its data layers (cockpit spec `views.json` and
// `layers.json`): which view is on screen, the layer level (Instruments /
// Full / Route / Off), and the per-layer flags, resolved to the set of layers
// drawn now.

import layersJson from "@/shared/cockpit-spec/layers.json";
import viewsJson from "@/shared/cockpit-spec/views.json";
import type { LayerLevel } from "@/shared/hud/ar-layout";

export type { LayerLevel };

/** The cockpit views, in the order Triangle (or `V`) cycles them. */
export const COCKPIT_VIEWS = ["pilot", "map", "split", "vision"] as const;
export type CockpitView = (typeof COCKPIT_VIEWS)[number];

/** The levels in the spec's order (`levels.order`), most to least drawn; the
 *  level button, `O` and the D-pad step through all of them and wrap. */
export const LAYER_LEVELS: readonly LayerLevel[] = layersJson.levels.order as LayerLevel[];

const VIEW_CYCLE: readonly CockpitView[] = (viewsJson.cycle as string[]).filter((v): v is CockpitView =>
  (COCKPIT_VIEWS as readonly string[]).includes(v),
);

/** Built-in layer ids and whether each is on by default. Plugin overlay
 *  layers (`plugin:<plugin_id>:<overlay_id>`) default on. */
export const LAYER_DEFAULTS: Readonly<Record<string, boolean>> = Object.fromEntries(
  layersJson.layers.map((l) => [l.id, l.default]),
);

/** The built-in layers in the spec's order, with their labels: the Layers
 *  ▾ menu lists one checkbox per flag. */
export const BUILTIN_LAYERS: readonly { id: string; label: string }[] = layersJson.layers.map((l) => ({ id: l.id, label: l.label }));

/** Layers the Route level keeps. */
export const ROUTE_LAYERS: readonly string[] = layersJson.levels.route.layers;
/** Layers the Off level keeps. */
export const OFF_LAYERS: readonly string[] = layersJson.levels.off.layers;
/** Leased layers (depth, surfaces) are toggled on their own; the level never
 *  turns them on or off. */
export const LEASED_LAYERS: readonly string[] = layersJson.layers.filter((l) => l.lease).map((l) => l.id);
/** Layers drawn only at the listed levels (`levels.only_at_levels`): the
 *  flight instruments draw only at the Instruments level. */
export const ONLY_AT_LEVELS: Readonly<Record<string, readonly string[]>> = layersJson.levels.only_at_levels;

/** Layers a view forces on while it is open. */
export const VIEW_FORCED_LAYERS: Readonly<Record<string, readonly string[]>> = Object.fromEntries(
  viewsJson.views.map((v) => [v.id, v.forced_layers]),
);

export const VIEW_LABELS: Readonly<Record<string, string>> = Object.fromEntries(
  viewsJson.views.map((v) => [v.id, v.label]),
);

export function isPluginLayer(id: string): boolean {
  return id.startsWith("plugin:");
}

/** Whether a layer's flag is on: the stored flag, else its default. */
export function layerFlag(layers: Readonly<Record<string, boolean>>, id: string): boolean {
  const v = layers[id];
  if (typeof v === "boolean") return v;
  return LAYER_DEFAULTS[id] ?? isPluginLayer(id);
}

/**
 * The layers drawn now. Full draws every layer whose flag is on; Route draws
 * the route set of those; Off draws the Off set. A layer listed in
 * `only_at_levels` (the flight instruments) draws only at those levels.
 * Leased layers (depth, surfaces) follow their own flag at every level. A
 * view's forced layers are added on top. `known` lists plugin layer ids
 * present on this vehicle so Full can include them.
 */
export function effectiveLayers(
  level: LayerLevel,
  layers: Readonly<Record<string, boolean>>,
  view: CockpitView,
  known: readonly string[] = [],
): Set<string> {
  const out = new Set<string>();
  const ids = new Set<string>([...Object.keys(LAYER_DEFAULTS), ...known, ...Object.keys(layers)]);
  for (const id of ids) {
    if (!layerFlag(layers, id)) continue;
    if (LEASED_LAYERS.includes(id)) {
      out.add(id);
      continue;
    }
    const only = ONLY_AT_LEVELS[id];
    if (only && !only.includes(level)) continue;
    if (level === "route" && !ROUTE_LAYERS.includes(id)) continue;
    if (level === "off" && !OFF_LAYERS.includes(id)) continue;
    out.add(id);
  }
  for (const id of OFF_LAYERS) out.add(id);
  for (const id of VIEW_FORCED_LAYERS[view] ?? []) out.add(id);
  return out;
}

/** The label of `view` when it forces layer `id` on while open (views.json
 *  `forced_layers`), else null. */
export function forcingView(view: CockpitView, id: string): string | null {
  return (VIEW_FORCED_LAYERS[view] ?? []).includes(id) ? (VIEW_LABELS[view] ?? view) : null;
}

export function nextView(view: CockpitView): CockpitView {
  return VIEW_CYCLE[(VIEW_CYCLE.indexOf(view) + 1) % VIEW_CYCLE.length];
}

export function nextLevel(level: LayerLevel): LayerLevel {
  return LAYER_LEVELS[(LAYER_LEVELS.indexOf(level) + 1) % LAYER_LEVELS.length];
}

export function prevLevel(level: LayerLevel): LayerLevel {
  return LAYER_LEVELS[(LAYER_LEVELS.indexOf(level) + LAYER_LEVELS.length - 1) % LAYER_LEVELS.length];
}

/** The data-layer id of a plugin's video overlay (`layers.json`
 *  `plugin_layer.id_pattern`). */
export function pluginLayerId(pluginId: string, overlayId: string): string {
  return `plugin:${pluginId}:${overlayId}`;
}
