// The shared overlay frame under every sheet, dialog and menu: a portal over
// the whole cockpit (CS-1 L4), a scrim that dismisses on tap, Escape and the
// controller back command dismissing too, focus trapped inside and returned
// on close, and the panel buttons / gamepad moving focus between its controls
// through the command stack, so nothing underneath moves while it is open.

import { useEffect, useRef, type CSSProperties, type ReactNode, type RefObject } from "react";
import { createPortal } from "react-dom";

import { pushCommandHandler } from "@/lib/command-stack";
import { Z } from "@/lib/cockpit-layout";
import { activateFocused, firstFocusable, moveFocus, trackFocusRing, trapTab } from "@/lib/focus-nav";
import { cn } from "@/lib/utils";

export interface OverlayProps {
  onDismiss: () => void;
  /** The id of the element that titles the panel. */
  labelledBy?: string;
  /** Focused first; defaults to the panel's first control. */
  initialFocus?: RefObject<HTMLElement | null>;
  /** A dimmed scrim (sheets, dialogs) or a clear one (menus). */
  scrim?: "dim" | "clear";
  /** Positions the panel inside the full-window layer. */
  layerClassName?: string;
  panelClassName?: string;
  panelStyle?: CSSProperties;
  children: ReactNode;
}

export function Overlay({
  onDismiss,
  labelledBy,
  initialFocus,
  scrim = "dim",
  layerClassName,
  panelClassName,
  panelStyle,
  children,
}: OverlayProps) {
  const panelRef = useRef<HTMLDivElement>(null);
  const dismissRef = useRef(onDismiss);
  useEffect(() => {
    dismissRef.current = onDismiss;
  });

  // Focus moves in on open and returns where it was on close; the focused
  // control carries the shared ring.
  useEffect(() => {
    const panel = panelRef.current;
    if (!panel) return;
    const previous = document.activeElement as HTMLElement | null;
    const stopRing = trackFocusRing(panel);
    (firstFocusable(initialFocus?.current ?? null) ?? firstFocusable(panel))?.focus();
    return () => {
      stopRing();
      previous?.focus?.();
    };
    // Focus is placed once, on open.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // The controller commands drive this overlay while it is the newest one.
  useEffect(
    () =>
      pushCommandHandler((cmd) => {
        const panel = panelRef.current;
        if (!panel) return false;
        switch (cmd) {
          case "back":
            dismissRef.current();
            return true;
          case "prev":
            moveFocus(panel, -1);
            return true;
          case "next":
            moveFocus(panel, 1);
            return true;
          case "activate":
            activateFocused(panel);
            return true;
          case "cycle-tab":
            return true;
          case "quick-menu":
            return false;
        }
      }),
    [],
  );

  return createPortal(
    <div className={cn("fixed inset-0", Z.sheets, layerClassName)}>
      <div
        className={cn("absolute inset-0", scrim === "dim" ? "bg-scrim" : "bg-transparent")}
        onClick={() => dismissRef.current()}
        aria-hidden
      />
      <div
        ref={panelRef}
        role="dialog"
        aria-modal="true"
        aria-labelledby={labelledBy}
        className={cn("relative", panelClassName)}
        style={panelStyle}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            dismissRef.current();
            return;
          }
          if (panelRef.current) trapTab(panelRef.current, e.nativeEvent);
        }}
      >
        {children}
      </div>
    </div>,
    document.body,
  );
}
