// The entry points to the Aircraft setup screen (Settings › Aircraft setup)
// for callers outside Settings: the flight-state chip's NOT READY "Set up
// aircraft" link, and the one-time offer banner. The screen is a detail
// screen, so it opens above whatever tab is active and the hardware back
// returns there.
//
// The offer: when the agent's setup engine reports a flight controller (by
// its uid) whose ground wizard is not done and that uid was never offered on
// this device, the shell shows a non-modal "Set up this aircraft" banner on
// the ground. Open or Later both mark the uid offered, in localStorage
// `ados-cockpit-setup-offered` (the kiosk keeps it in its persistent profile).

import { setupNeeded } from "@/shared/setup-wizard/parse";
import type { SetupStatus } from "@/shared/setup-wizard/types";
import { useNavStore } from "@/stores/nav-store";

/** The registry id of the Aircraft setup screen. */
export const AIRCRAFT_SETUP_SCREEN_ID = "aircraft-setup";

/** Open the Aircraft setup screen (a no-op when it is already on top). */
export function openAircraftSetup(): void {
  const nav = useNavStore.getState();
  if (nav.detailStack[nav.detailStack.length - 1] === AIRCRAFT_SETUP_SCREEN_ID) return;
  nav.openDetail(AIRCRAFT_SETUP_SCREEN_ID);
}

export const SETUP_OFFERED_KEY = "ados-cockpit-setup-offered";

/** The flight-controller uids this device has offered setup for. */
export function loadOfferedUids(): string[] {
  if (typeof localStorage === "undefined") return [];
  try {
    const raw = localStorage.getItem(SETUP_OFFERED_KEY);
    const v: unknown = raw ? JSON.parse(raw) : [];
    return Array.isArray(v) ? v.filter((s): s is string => typeof s === "string") : [];
  } catch {
    return [];
  }
}

/** Remember that setup was offered for `uid`; returns the new list. */
export function markSetupOffered(uid: string): string[] {
  const list = loadOfferedUids();
  if (list.includes(uid)) return list;
  const next = [...list, uid];
  try {
    localStorage.setItem(SETUP_OFFERED_KEY, JSON.stringify(next));
  } catch {
    // storage blocked or full: the offer stays dismissed for this session
  }
  return next;
}

/** The flight-controller uid to offer setup for now, or null: an identified
 *  FC whose ground wizard is not done, never offered on this device, and the
 *  vehicle on the ground. */
export function setupOfferUid(status: SetupStatus | null, offered: readonly string[], airborne: boolean): string | null {
  if (airborne || !status?.uid || !setupNeeded(status)) return null;
  return offered.includes(status.uid) ? null : status.uid;
}
