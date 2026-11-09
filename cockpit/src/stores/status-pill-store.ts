// The status pill's acknowledgements: the alert ids the pilot has seen. The
// pill flashes while an active alert is not in this set; opening the alert
// list acknowledges every alert shown. Ids of alerts that have cleared are
// dropped on the next acknowledgement, so an alert that clears and returns
// flashes again.

import { create } from "zustand";

interface StatusPillState {
  acked: ReadonlySet<string>;
  /** Acknowledge exactly these (the active) alert ids. */
  ack: (ids: readonly string[]) => void;
}

export const useStatusPillStore = create<StatusPillState>((set) => ({
  acked: new Set<string>(),
  ack: (ids) => set({ acked: new Set(ids) }),
}));
