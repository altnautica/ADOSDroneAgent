// The window width class and orientation, re-read on resize. One listener for
// the whole app; every subscriber reads the same snapshot. Inside a box that
// publishes its own measure (`BoxViewport`, the Feed beside the menu rail),
// the class and orientation come from that box instead.

import { createContext, useContext, useLayoutEffect, useState, useSyncExternalStore, type RefObject } from "react";

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

/** The viewport of the enclosing box, when one publishes it. */
export const BoxViewport = createContext<Viewport | null>(null);

export function useWidthClass(): Viewport {
  const box = useContext(BoxViewport);
  const win = useSyncExternalStore(subscribe, read, () => FALLBACK);
  return box ?? win;
}

/** The width class and orientation of an element's own box, re-measured as
 *  it resizes; null until measured or while `on` is false. */
export function useBoxViewport(ref: RefObject<HTMLElement | null>, on: boolean): Viewport | null {
  const [viewport, setViewport] = useState<Viewport | null>(null);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!on || !el) {
      setViewport(null);
      return;
    }
    const measure = (w: number, h: number) =>
      setViewport((v) => {
        const widthClass = widthClassFor(w, isKiosk());
        const portrait = h > w;
        return v && v.widthClass === widthClass && v.portrait === portrait ? v : { widthClass, portrait };
      });
    const r = el.getBoundingClientRect();
    measure(r.width, r.height);
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(([entry]) => measure(entry.contentRect.width, entry.contentRect.height));
    ro.observe(el);
    return () => ro.disconnect();
  }, [ref, on]);
  return viewport;
}
