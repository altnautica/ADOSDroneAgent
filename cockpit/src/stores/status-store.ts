// The node status the whole shell reads: the status strip, the Feed band,
// alerts and the screens. One poller (`useStatusPoll`, mounted once by the
// shell) writes it; consumers subscribe with field selectors so a poll only
// re-renders what actually changed.

import { create } from "zustand";

import type { BatteryHealth, GsStatus } from "@/lib/types";

export interface StatusState {
  status: GsStatus | null;
  /** True when the most recent status poll failed (the snapshot may be old). */
  stale: boolean;
  error: string | null;
  /** The battery engine read model, null when the node has no battery engine
   *  or it has not answered yet. */
  battery: BatteryHealth | null;
}

export const useStatusStore = create<StatusState>(() => ({
  status: null,
  stale: false,
  error: null,
  battery: null,
}));
