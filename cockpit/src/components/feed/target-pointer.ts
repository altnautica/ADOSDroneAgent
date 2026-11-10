// Pure pieces of the target layer: the pointer mapping into layout pixels and
// the "Targets unavailable" rule.

import type { DetectorState } from "@/lib/detection-lease";
import type { Point } from "@/shared/hud/target-layout";
import { targetsUnavailable, type BatchStage } from "@/stores/detections-store";

interface LayoutBox {
  clientWidth: number;
  clientHeight: number;
  getBoundingClientRect(): { left: number; top: number; width: number; height: number };
}

/**
 * A client point in the element's own layout pixels. The digital zoom scales
 * the layer with a CSS transform, so the on-screen rect is scaled while the
 * layout size (what the boxes are laid out in) is not; the ratio undoes it.
 */
export function layoutPoint(clientX: number, clientY: number, el: LayoutBox): Point {
  const r = el.getBoundingClientRect();
  const sx = r.width > 0 ? el.clientWidth / r.width : 1;
  const sy = r.height > 0 ? el.clientHeight / r.height : 1;
  return { x: (clientX - r.left) * sx, y: (clientY - r.top) * sy };
}

/** "Targets unavailable" shows only while the detector is detecting, or,
 *  with its state unknown, once a batch was picked; never beside "Detector
 *  idle" or "Detector unavailable". */
export function showTargetsUnavailable(stage: BatchStage, detector: DetectorState | null, picked: boolean): boolean {
  return targetsUnavailable(stage, detector === "detecting" || (detector === null && picked));
}
