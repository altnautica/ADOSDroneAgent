// Which drafts on screen hold unsaved changes. Each form or field registers
// its own dirty flag under a stable id; the app's single navigation guard
// reads whether any is dirty.

export interface DirtyRegistry {
  set(id: string, dirty: boolean): void;
  remove(id: string): void;
  anyDirty(): boolean;
  subscribe(listener: () => void): () => void;
}

export function createDirtyRegistry(): DirtyRegistry {
  const dirty = new Set<string>();
  const listeners = new Set<() => void>();
  const notify = () => listeners.forEach((l) => l());
  return {
    set(id, value) {
      if (value === dirty.has(id)) return;
      if (value) dirty.add(id);
      else dirty.delete(id);
      notify();
    },
    remove(id) {
      if (dirty.delete(id)) notify();
    },
    anyDirty: () => dirty.size > 0,
    subscribe(listener) {
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
}

/** The app's registry. */
export const dirtyRegistry = createDirtyRegistry();
