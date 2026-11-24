// The data layers' status chips ("Obstacles unavailable", "Depth unscaled",
// "Targets unavailable"): one bit per `CHIP_IDS` entry. The layer engine
// reports its set (written only when it changes); the targets layer, drawn
// outside the engine, reports its own chip. `mask` is the union the Feed
// stacks under its banners and alert cards, so no banner can cover one.
//
// The vision engine's last `GET /api/vision/status` read lives here too
// (written by the one shared poller, `useVisionEngine`): the chips read it to
// say whether a layer is missing because the engine is off, and the Vision
// panel's engine card reads the same answer.

import { create } from "zustand";

import { CHIP_IDS } from "@/shared/hud/layer-engine";

const TARGETS_BIT = 1 << CHIP_IDS.indexOf("targets");

export interface VisionEngineModel {
  id: string;
  kind: string;
  fps: number | null;
  latency_ms: number | null;
  backend_loaded: boolean;
}

/** The fields of `GET /api/vision/status` the cockpit reads. */
export interface VisionEngineStatus {
  backend: string | null;
  /** `ready`, `mock` or `unavailable` (the backend cannot run here). */
  backend_state: string;
  backend_reason: string | null;
  /** The requested backend is not in this agent build: no restart brings it
   *  up. Absent from an older agent. */
  backend_runtime_missing?: boolean;
  models: VisionEngineModel[];
  /** The detector's demand state; its `cameras` names every camera the
   *  engine runs. Null when the engine wrote no status sidecar. */
  detection?: { cameras: Record<string, unknown> } | null;
}

interface LayerChipsState {
  mask: number;
  /** The layer engine's chips. */
  engineMask: number;
  /** The targets layer's "Targets unavailable" chip. */
  targets: boolean;
  setMask: (mask: number) => void;
  setTargetsUnavailable: (on: boolean) => void;
  /** The vision engine's last status, null before the first answer. */
  vision: VisionEngineStatus | null;
  /** The last status read failed (the engine is off or not up: a 503). */
  visionError: boolean;
  setVision: (status: VisionEngineStatus | null, error: boolean) => void;
}

function union(engineMask: number, targets: boolean): number {
  return targets ? engineMask | TARGETS_BIT : engineMask & ~TARGETS_BIT;
}

export const useLayerChipsStore = create<LayerChipsState>((set, get) => ({
  mask: 0,
  engineMask: 0,
  targets: false,
  setMask: (engineMask) => set({ engineMask, mask: union(engineMask, get().targets) }),
  setTargetsUnavailable: (targets) => {
    if (get().targets !== targets) set({ targets, mask: union(get().engineMask, targets) });
  },
  vision: null,
  visionError: false,
  // A failed read keeps the last status for its detail; `visionError` says
  // it is no longer current.
  setVision: (status, error) => set({ vision: status ?? get().vision, visionError: error }),
}));

/** Whether the vision engine runs: false when its status read fails or it
 *  reports its backend unavailable, null before the first answer. */
export function visionEngineUp(s: Pick<LayerChipsState, "vision" | "visionError">): boolean | null {
  if (s.visionError) return false;
  if (s.vision === null) return null;
  return s.vision.backend_state !== "unavailable";
}
