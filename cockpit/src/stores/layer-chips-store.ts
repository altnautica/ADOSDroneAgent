// The data layers' status chips ("Obstacles unavailable", "Depth unscaled"),
// as the layer engine reports them: one bit per `CHIP_IDS` entry, written
// only when the set changes. The Feed stacks them under its banners and alert
// cards, so no banner can cover one.

import { create } from "zustand";

interface LayerChipsState {
  mask: number;
  setMask: (mask: number) => void;
}

export const useLayerChipsStore = create<LayerChipsState>((set) => ({
  mask: 0,
  setMask: (mask) => set({ mask }),
}));
