// Mark a screen as holding unsaved drafts. While `dirty` is true the in-app
// navigator asks before leaving (the shell's discard dialog), and the browser
// prompts on tab close or reload. Unmounting clears the mark.

import { useEffect, useId } from "react";

import { useDirtyStore } from "@/stores/dirty-store";

export function useDirtyGuard(dirty: boolean, owner?: string): void {
  const fallback = useId();
  const id = owner ?? fallback;
  const setDirty = useDirtyStore((s) => s.setDirty);

  useEffect(() => {
    setDirty(id, dirty);
    return () => setDirty(id, false);
  }, [id, dirty, setDirty]);

  useEffect(() => {
    if (!dirty) return;
    const onBeforeUnload = (e: BeforeUnloadEvent) => {
      e.preventDefault();
      // Setting returnValue is what makes the browser prompt.
      e.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);
    return () => window.removeEventListener("beforeunload", onBeforeUnload);
  }, [dirty]);
}
