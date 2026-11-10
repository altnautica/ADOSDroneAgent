// Which cockpit sheet is open (Mode sheet, Launch sheet, pre-flight
// checklist), and the bottom-left card shown. One sheet at a time: opening one
// closes the other. Also the stack of every open overlay (kept by the shared
// overlay frame) and the launch Abort, which the Launch sheet and the flight
// overlays both offer.

import type { RefObject } from "react";
import { create } from "zustand";

import { sendPilotAction } from "@/lib/pilot-link";
import { refusalText } from "@/shared/hud/refusal";
import { specText } from "@/shared/hud/spec-text";

/** One open overlay (sheet, dialog, menu), as `components/ui/overlay` keeps it. */
export interface OpenOverlay {
  dismiss: () => void;
  panel: RefObject<HTMLDivElement | null>;
  /** Opened from inside another overlay, so it stacks on it. */
  nested: boolean;
}

/** The overlays open now, oldest first. */
export const openOverlays: OpenOverlay[] = [];

/** Whether any overlay is open (the controller then drives it, not flight). */
export function overlayOpen(): boolean {
  return openOverlays.length > 0;
}

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
  /** A go-to point (metres north/east of the vehicle) the Launch sheet opens
   *  with, set by a long-press on a map while on the ground. */
  launchGoto: { n: number; e: number } | null;
  /** Open the Launch sheet, prefilled with a go-to point when given. */
  openLaunch: (goto?: { n: number; e: number } | null) => void;
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
  launchGoto: null,
  openLaunch: (goto = null) => set({ sheet: "launch", launchGoto: goto }),
  openSheet: (sheet) => set({ sheet, launchGoto: null }),
  closeSheet: () => set({ sheet: null, launchGoto: null }),
  nextCard: (dir = 1) => {
    const i = CARDS.indexOf(get().card);
    set({ card: CARDS[(i + dir + CARDS.length) % CARDS.length] });
  },
  setShutterMode: (shutterMode) => set({ shutterMode }),
}));

/** Abort a running launch: the pilot brakes to a hold, or disarms while still
 *  on the ground. A refusal shows in the Launch sheet. */
export function abortLaunch(): void {
  void sendPilotAction("pause").then((r) =>
    useCockpitUiStore.getState().setLaunchRefusal(r.ok ? null : specText("flow.launch.abort_refused", { reason: refusalText(r.err) })),
  );
}
