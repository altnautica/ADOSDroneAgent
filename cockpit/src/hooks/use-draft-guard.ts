// The dirty guard for a screen whose drafts live in a session store (they
// outlive the screen): registers with the shell's in-app discard dialog and the
// browser unload prompt, and when the operator chooses "discard" in that dialog
// the store's drafts are actually dropped.

import { useEffect, useRef } from "react";

import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { useDirtyStore } from "@/stores/dirty-store";

export function useDraftGuard(dirty: boolean, owner: string, discard: () => void): void {
  useDirtyGuard(dirty, owner);
  const discardRef = useRef(discard);
  useEffect(() => {
    discardRef.current = discard;
  });
  useEffect(
    () =>
      useDirtyStore.subscribe((s, prev) => {
        // discardAndLeave clears the parked leave and every owner at once;
        // keepEditing clears only the parked leave.
        if (prev.pending && !s.pending && prev.owners.includes(owner) && !s.owners.includes(owner)) {
          discardRef.current();
        }
      }),
    [owner],
  );
}
