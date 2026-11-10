// How many top-bar items the bar has moved into the Menu right now (in the
// order of `TOP_BAR_OVERFLOW`). The top bar writes it as its row runs out of
// room; the Menu reads it to list only the items actually hidden.

import { create } from "zustand";

interface TopBarState {
  movedCount: number;
  setMovedCount: (n: number) => void;
}

export const useTopBarStore = create<TopBarState>((set) => ({
  movedCount: 0,
  setMovedCount: (movedCount) => set({ movedCount }),
}));
