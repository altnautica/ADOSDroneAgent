// The in-app dirty guard. A screen holding unsaved drafts registers itself
// here; the navigator asks before leaving (back, tab switch), and while any
// draft exists the leave is parked and the shell shows a dialog. Discarding
// clears every draft and runs the parked leave; keeping editing drops it.
//
// Browser tab close and reload are covered separately by `beforeunload` in
// `hooks/use-dirty-guard.ts`.

import { create } from "zustand";

interface DirtyState {
  /** Owner ids with unsaved drafts. */
  owners: string[];
  /** The navigation waiting on the operator's answer, if any. */
  pending: (() => void) | null;
  setDirty: (owner: string, dirty: boolean) => void;
  /** Run `leave` now when nothing is dirty and return true; otherwise park it
   *  for the dialog and return false. */
  requestLeave: (leave: () => void) => boolean;
  discardAndLeave: () => void;
  keepEditing: () => void;
}

export const useDirtyStore = create<DirtyState>((set, get) => ({
  owners: [],
  pending: null,

  setDirty: (owner, dirty) =>
    set((s) => {
      const has = s.owners.includes(owner);
      if (dirty === has) return s;
      return { owners: dirty ? [...s.owners, owner] : s.owners.filter((o) => o !== owner) };
    }),

  requestLeave: (leave) => {
    if (get().owners.length === 0) {
      leave();
      return true;
    }
    set({ pending: leave });
    return false;
  },

  discardAndLeave: () => {
    const leave = get().pending;
    set({ owners: [], pending: null });
    leave?.();
  },

  keepEditing: () => set({ pending: null }),
}));

/** Whether any screen holds unsaved drafts. */
export function hasDrafts(): boolean {
  return useDirtyStore.getState().owners.length > 0;
}
