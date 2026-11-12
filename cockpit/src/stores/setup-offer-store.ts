// The one-time aircraft setup offer, shared by the shell banner and the Menu:
// the flight-controller uid on offer now (the banner sets it from the setup
// engine's poll) and the uids this device already offered. The Menu's "Set up
// aircraft" row carries the same offer to panel and controller users, who
// cannot reach a banner button; opening it from either place marks it offered.

import { create } from "zustand";

import { loadOfferedUids, markSetupOffered, openAircraftSetup } from "@/lib/aircraft-setup";

interface SetupOfferState {
  /** The uid on offer, or null when there is no offer. */
  uid: string | null;
  offered: string[];
  setUid: (uid: string | null) => void;
  /** Close the offer (Open or Later); `open` also opens Aircraft setup. */
  close: (open: boolean) => void;
}

export const useSetupOfferStore = create<SetupOfferState>((set, get) => ({
  uid: null,
  offered: loadOfferedUids(),
  setUid: (uid) => set({ uid }),
  close: (open) => {
    const { uid } = get();
    if (uid) set({ uid: null, offered: markSetupOffered(uid) });
    if (open) openAircraftSetup();
  },
}));
