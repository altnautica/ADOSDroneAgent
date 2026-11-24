// A small anchored panel (the strip's `⋯` readings) closes the way every
// popover does: on Escape, and on a pointer-down anywhere outside its anchor
// (the toggle and the panel). The press outside still reaches what it lands
// on; the panel holds nothing that needs confirming.

import { useEffect, type RefObject } from "react";

export function usePanelDismiss(open: boolean, anchor: RefObject<HTMLElement | null>, close: () => void): void {
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      // Consumed here, so the shell's own Escape (back) does not also run.
      e.preventDefault();
      e.stopPropagation();
      close();
    };
    const onPointer = (e: PointerEvent) => {
      const t = e.target as Node | null;
      if (t && anchor.current?.contains(t)) return;
      close();
    };
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("pointerdown", onPointer, true);
    return () => {
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("pointerdown", onPointer, true);
    };
  }, [open, anchor, close]);
}
