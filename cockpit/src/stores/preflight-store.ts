// The pre-flight checklist gate: the first arm of a session (arm, takeoff,
// launch) opens the checklist; once the operator has passed it, later arms
// fire directly. A session is the page's lifetime. Passing reopens the sheet
// the checklist replaced (the Launch sheet), so its step strip, Abort and any
// refusal stay in view.

import { create } from "zustand";

import { useCockpitUiStore, type CockpitSheet } from "@/stores/cockpit-ui-store";

interface PreflightState {
  /** The operator passed the checklist this session. */
  passed: boolean;
  /** The action waiting behind the checklist. */
  pending: (() => void) | null;
  /** The sheet the checklist replaced, reopened when it passes. */
  returnTo: CockpitSheet;
  /** The Launch sheet's go-to prefill, kept for its reopening. */
  returnGoto: { n: number; e: number } | null;
  pass: () => void;
  dismiss: () => void;
}

export const usePreflightStore = create<PreflightState>((set, get) => ({
  passed: false,
  pending: null,
  returnTo: null,
  returnGoto: null,
  pass: () => {
    const { pending: then, returnTo, returnGoto } = get();
    set({ passed: true, pending: null, returnTo: null, returnGoto: null });
    const ui = useCockpitUiStore.getState();
    if (returnTo === "launch") ui.openLaunch(returnGoto);
    else if (returnTo) ui.openSheet(returnTo);
    else ui.closeSheet();
    then?.();
  },
  dismiss: () => {
    set({ pending: null, returnTo: null, returnGoto: null });
    useCockpitUiStore.getState().closeSheet();
  },
}));

/** Run `then` now if the checklist was passed this session; otherwise open the
 *  checklist and run it when the operator passes. */
export function requirePreflight(then: () => void): void {
  if (usePreflightStore.getState().passed) {
    then();
    return;
  }
  const ui = useCockpitUiStore.getState();
  usePreflightStore.setState({ pending: then, returnTo: ui.sheet === "preflight" ? null : ui.sheet, returnGoto: ui.launchGoto });
  ui.openSheet("preflight");
}
