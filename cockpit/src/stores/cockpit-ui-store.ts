// Which cockpit sheet is open (Mode sheet, Launch sheet, pre-flight
// checklist), and the bottom-left card shown. One sheet at a time: opening one
// closes the other.

import { create } from "zustand";

export type CockpitSheet = "mode" | "launch" | "preflight" | null;
export type BottomLeftCard = "map" | "attitude_radar" | "obstacle_radar";

const CARDS: readonly BottomLeftCard[] = ["map", "attitude_radar", "obstacle_radar"];

interface CockpitUiState {
  sheet: CockpitSheet;
  card: BottomLeftCard;
  /** The shutter's mode: a still photo or a video recording. */
  shutterMode: "photo" | "video";
  /** The pilot's refusal of the last launch, kept here so it survives the
   *  Launch sheet unmounting behind the pre-flight checklist. */
  launchRefusal: string | null;
  setLaunchRefusal: (text: string | null) => void;
  openSheet: (sheet: CockpitSheet) => void;
  closeSheet: () => void;
  nextCard: (dir?: 1 | -1) => void;
  setShutterMode: (mode: "photo" | "video") => void;
}

export const useCockpitUiStore = create<CockpitUiState>((set, get) => ({
  sheet: null,
  card: "map",
  shutterMode: "video",
  launchRefusal: null,
  setLaunchRefusal: (launchRefusal) => set({ launchRefusal }),
  openSheet: (sheet) => set({ sheet }),
  closeSheet: () => set({ sheet: null }),
  nextCard: (dir = 1) => {
    const i = CARDS.indexOf(get().card);
    set({ card: CARDS[(i + dir + CARDS.length) % CARDS.length] });
  },
  setShutterMode: (shutterMode) => set({ shutterMode }),
}));
