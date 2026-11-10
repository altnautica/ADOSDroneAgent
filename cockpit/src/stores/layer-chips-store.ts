// The data layers' status chips ("Obstacles unavailable", "Depth unscaled",
// "Targets unavailable"): one bit per `CHIP_IDS` entry. The layer engine
// reports its set (written only when it changes); the targets layer, drawn
// outside the engine, reports its own chip. `mask` is the union the Feed
// stacks under its banners and alert cards, so no banner can cover one.

import { create } from "zustand";

import { CHIP_IDS } from "@/shared/hud/layer-engine";

const TARGETS_BIT = 1 << CHIP_IDS.indexOf("targets");

interface LayerChipsState {
  mask: number;
  /** The layer engine's chips. */
  engineMask: number;
  /** The targets layer's "Targets unavailable" chip. */
  targets: boolean;
  setMask: (mask: number) => void;
  setTargetsUnavailable: (on: boolean) => void;
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
}));
