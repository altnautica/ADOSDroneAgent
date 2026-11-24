// The cockpit's side of the alert centre: where a row's fix goes, and the
// pill's open request and warning count, so the flight-state chip's
// "Warnings (n)" can open the centre under the pill.

import { create } from "zustand";

import { openAircraftSetup } from "@/lib/aircraft-setup";
import { openParameters } from "@/lib/param-filter";
import type { AlertActions } from "@/shared/hud/alert-centre";
import type { AgentProfile } from "@/shared/use-profile";
import { useNavStore } from "@/stores/nav-store";

/** Where the cockpit's alert fixes go. A ground station has no setup engine
 *  of its own (its rows show the drone's readiness): setup fixes read as a
 *  hint. The parameter editor is a drone tab, opened filtered to the names.
 *  Each action closes the list (`done`) before it navigates. */
export function cockpitAlertActions(profile: AgentProfile | null, done: () => void): AlertActions {
  return {
    openSetup:
      profile === "ground_station"
        ? null
        : (step) => {
            done();
            openAircraftSetup(step);
          },
    openSettings: (section) => {
      done();
      useNavStore.getState().openDetail(`settings:!${section}`);
    },
    openParameters:
      profile === "drone" || profile === "visor"
        ? (names) => {
            done();
            openParameters(names);
          }
        : null,
  };
}

/** The pill's warning count and its open request (bumped to open). */
export const useAlertCentreStore = create<{ warnings: number; openSeq: number; open: () => void }>((set) => ({
  warnings: 0,
  openSeq: 0,
  open: () => set((s) => ({ openSeq: s.openSeq + 1 })),
}));
