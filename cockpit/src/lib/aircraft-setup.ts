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

import { useSyncExternalStore } from "react";

import { BATTERY_FAILSAFE_FIX_STEP } from "@/shared/hud/battery-failsafe-fix";
import { rerunStep } from "@/shared/setup-wizard/client";
import { setupNeeded } from "@/shared/setup-wizard/parse";
import type { SetupStatus } from "@/shared/setup-wizard/types";
import type { OpenStepRequest } from "@/shared/setup-wizard/wizard-view";
import { useNavStore } from "@/stores/nav-store";

/** The registry id of the Aircraft setup screen. */
export const AIRCRAFT_SETUP_SCREEN_ID = "aircraft-setup";

/** The step the Aircraft setup screen should open, set by the last open that
 *  named one. Each open gets a new `seq`, so the screen opens it even when
 *  it is already showing (an alert's "Open setup" over the open screen). */
let pending: OpenStepRequest | null = null;
let seq = 0;
const listeners = new Set<() => void>();

function setPending(next: OpenStepRequest | null): void {
  pending = next;
  for (const l of listeners) l();
}

/** Open the Aircraft setup screen (a no-op when it is already on top); with
 *  `step`, the screen opens that step (`usePendingSetupStep`). An open
 *  without a step clears any step left pending. */
export function openAircraftSetup(step?: string): void {
  seq += 1;
  setPending(step ? { step, seq } : null);
  const nav = useNavStore.getState();
  if (nav.detailStack[nav.detailStack.length - 1] === AIRCRAFT_SETUP_SCREEN_ID) return;
  nav.openDetail(AIRCRAFT_SETUP_SCREEN_ID);
}

/** The open request in hand, or null. */
export function pendingSetupStep(): OpenStepRequest | null {
  return pending;
}

/** The screen acted on request `seq`: drop it, unless a newer one came. */
export function clearPendingSetupStep(handled: number): void {
  if (pending?.seq === handled) setPending(null);
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

/** The open request, live: a mounted screen sees each new one. */
export function usePendingSetupStep(): OpenStepRequest | null {
  return useSyncExternalStore(subscribe, pendingSetupStep, pendingSetupStep);
}

/** The battery failsafe fix: open Aircraft setup and run the ground
 *  wizard's ADOS profile step, which lists the failsafe profile and writes
 *  nothing until the operator applies it there. The screen's setup stream
 *  shows the step's result; a refused run reads on that step as before. */
export function fixBatteryFailsafe(): void {
  openAircraftSetup();
  rerunStep(BATTERY_FAILSAFE_FIX_STEP.wizard, BATTERY_FAILSAFE_FIX_STEP.step).catch(() => undefined);
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
