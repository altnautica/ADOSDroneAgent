// The cockpit's one mutable layer frame store: the latest `ar`, `obstacles`,
// `depth` and `surfaces` frames from the nav stream (or the demo feed),
// written outside React so a 20 Hz frame never re-renders a component. The
// LayerStack's frame loop reads it directly.

import { LayerFrameStore } from "@/shared/hud/layer-engine";

export const layerFrames = new LayerFrameStore();
