// The entry point to the Aircraft setup screen (Settings › Aircraft setup) for
// callers outside Settings: the flight-state chip's NOT READY "Set up
// aircraft" link. The screen is a detail screen, so it opens above whatever
// tab is active and the hardware back returns there.

import { useNavStore } from "@/stores/nav-store";

/** The registry id of the Aircraft setup screen. */
export const AIRCRAFT_SETUP_SCREEN_ID = "aircraft-setup";

/** Open the Aircraft setup screen (a no-op when it is already on top). */
export function openAircraftSetup(): void {
  const nav = useNavStore.getState();
  if (nav.detailStack[nav.detailStack.length - 1] === AIRCRAFT_SETUP_SCREEN_ID) return;
  nav.openDetail(AIRCRAFT_SETUP_SCREEN_ID);
}
