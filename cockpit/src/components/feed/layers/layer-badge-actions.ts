// What a layer badge's menu items do, apart from the menu component.

import { forcingView, type CockpitView } from "@/lib/cockpit-view";
import type { LayerChip } from "@/shared/hud/layer-engine";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";

/** The Settings section a badge's Set up opens. */
export const VISION_SETTINGS_SCREEN = "settings:!vision";

/** What a badge's first action does: switch the layer off, or close the view
 *  that forces it on (switching its flag off would change nothing there). */
export function badgeTurnOff(chip: LayerChip, view: CockpitView): "turn_off" | "close_view" {
  return chip.layers.some((id) => forcingView(view, id) !== null) ? "close_view" : "turn_off";
}

/** Run a badge's first action: close the forcing view (back to Pilot), or
 *  switch every layer flag the chip stands for off. */
export function turnOffBadge(chip: LayerChip, view: CockpitView): void {
  const s = useCockpitViewStore.getState();
  if (badgeTurnOff(chip, view) === "close_view") s.setView("pilot");
  else for (const id of chip.layers) s.toggleLayer(id, false);
}
