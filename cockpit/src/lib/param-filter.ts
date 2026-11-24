// The Parameters screen's name filter: an alert's "Open parameters" opens the
// screen showing only the parameters that alert names, under a "Filtered: N
// parameters · Clear" chip. The filter is a store, not a read-once value, so
// it also applies when the Parameters tab is already mounted; Clear drops it.

import { create } from "zustand";

import { useNavStore } from "@/stores/nav-store";

export const PARAMS_TAB_ID = "params";

export const useParamFilterStore = create<{ names: readonly string[] | null; clear: () => void }>((set) => ({
  names: null,
  clear: () => set({ names: null }),
}));

/** Open the Parameters tab showing only `names`. */
export function openParameters(names: readonly string[]): void {
  useParamFilterStore.setState({ names: names.length > 0 ? [...names] : null });
  useNavStore.getState().goTab(PARAMS_TAB_ID);
}

/** The rows a name filter keeps (all of them without one), by exact name. */
export function filterByNames<T extends { name: string }>(rows: readonly T[], names: readonly string[] | null): readonly T[] {
  if (!names) return rows;
  const keep = new Set(names);
  return rows.filter((r) => keep.has(r.name));
}
