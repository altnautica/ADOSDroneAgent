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
import { useConfirmStore } from "@/stores/confirm-store";

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
  // A refusal belongs to the press that drew it: a sheet opened later starts
  // clean (an Abort refused with the sheet closed already showed on the ack
  // line).
  openLaunch: (goto = null) => set({ sheet: "launch", launchGoto: goto, launchRefusal: null }),
  openSheet: (sheet) => set(sheet === "launch" ? { sheet, launchGoto: null, launchRefusal: null } : { sheet, launchGoto: null }),
  closeSheet: () => set({ sheet: null, launchGoto: null }),
  nextCard: (dir = 1) => {
    const i = CARDS.indexOf(get().card);
    set({ card: CARDS[(i + dir + CARDS.length) % CARDS.length] });
  },
  setShutterMode: (shutterMode) => set({ shutterMode }),
}));

const ABORT_ACK_CLEAR_MS = 4000;
let abortAckTimer: ReturnType<typeof setTimeout> | undefined;

/** Abort a running launch: the pilot brakes to a hold, or disarms while still
 *  on the ground. Abort is offered with the Launch sheet closed too (the chip,
 *  the flight overlay), so a refusal or a timeout always shows on the cockpit's
 *  ack line, and in the Launch sheet while it is open. */
export function abortLaunch(): void {
  void sendPilotAction("pause").then((r) => {
    const ui = useCockpitUiStore.getState();
    if (r.ok) {
      ui.setLaunchRefusal(null);
      return;
    }
    const text = specText("flow.launch.abort_refused", { reason: refusalText(r.err) });
    if (ui.sheet === "launch") ui.setLaunchRefusal(text);
    useConfirmStore.getState().setAck({ text, kind: "err" });
    clearTimeout(abortAckTimer);
    abortAckTimer = setTimeout(() => {
      if (useConfirmStore.getState().ack?.text === text) useConfirmStore.getState().setAck(null);
    }, ABORT_ACK_CLEAR_MS);
  });
}
