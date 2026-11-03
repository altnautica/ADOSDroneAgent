// The window width class and orientation, re-read on resize. One listener for
// the whole app; every subscriber reads the same snapshot.

import { useSyncExternalStore } from "react";

import { widthClassFor, type WidthClass } from "@/lib/cockpit-layout";
import { isKiosk } from "@/lib/kiosk";

export interface Viewport {
  widthClass: WidthClass;
  /** Taller than wide: the stacked layouts. */
  portrait: boolean;
}

const FALLBACK: Viewport = { widthClass: "medium", portrait: false };

let snapshot: Viewport | null = null;

function read(): Viewport {
  if (typeof window === "undefined") return FALLBACK;
  const next: Viewport = {
    widthClass: widthClassFor(window.innerWidth, isKiosk()),
    portrait: window.innerHeight > window.innerWidth,
  };
  // Keep the same object while nothing changed, so subscribers do not re-render
  // on a resize that stays inside one class.
  if (snapshot && snapshot.widthClass === next.widthClass && snapshot.portrait === next.portrait) {
    return snapshot;
  }
  snapshot = next;
  return next;
}

function subscribe(onChange: () => void): () => void {
  window.addEventListener("resize", onChange);
  return () => window.removeEventListener("resize", onChange);
}

export function useWidthClass(): Viewport {
  return useSyncExternalStore(subscribe, read, () => FALLBACK);
}
