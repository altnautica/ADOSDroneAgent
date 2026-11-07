// The pre-flight checklist gate (C5): the first arm of a session (arm,
// takeoff, launch) opens the checklist; once the operator has passed it, later
// arms fire directly. A session is the page's lifetime.

import { create } from "zustand";

import { useCockpitUiStore } from "@/stores/cockpit-ui-store";

interface PreflightState {
  /** The operator passed the checklist this session. */
  passed: boolean;
  /** The action waiting behind the checklist. */
  pending: (() => void) | null;
  pass: () => void;
  dismiss: () => void;
}

export const usePreflightStore = create<PreflightState>((set, get) => ({
  passed: false,
  pending: null,
  pass: () => {
    const then = get().pending;
    set({ passed: true, pending: null });
    useCockpitUiStore.getState().closeSheet();
    then?.();
  },
  dismiss: () => {
    set({ pending: null });
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
  usePreflightStore.setState({ pending: then });
  useCockpitUiStore.getState().openSheet("preflight");
}
