import { create } from "zustand";

// In-memory draft tracker for the parameter editor. Drafts persist
// across route navigation within the session so the operator can
// browse other panels and come back without losing edits, but they
// don't persist to localStorage (param values can change on the FC
// behind the dashboard's back; persisting would create stale drafts).

interface Draft {
  original: number;
  draft: number;
}

interface ParamsState {
  drafts: Map<string, Draft>;
  setDraft: (name: string, original: number, draft: number) => void;
  discardDraft: (name: string) => void;
  clearAll: () => void;
}

export const useParamsStore = create<ParamsState>()((set) => ({
  drafts: new Map(),
  setDraft: (name, original, draft) =>
    set((s) => {
      const next = new Map(s.drafts);
      // If draft equals original, drop the entry (no longer dirty)
      if (Math.abs(original - draft) < 1e-9) {
        next.delete(name);
      } else {
        next.set(name, { original, draft });
      }
      return { drafts: next };
    }),
  discardDraft: (name) =>
    set((s) => {
      const next = new Map(s.drafts);
      next.delete(name);
      return { drafts: next };
    }),
  clearAll: () => set({ drafts: new Map() }),
}));
