// The text scale and the top inset of a flight-screen surface that may sit
// outside the zoned chrome: a map filling the stage or the split pane, the
// instruments, the vision panel. Text is sized in CSS px times the class
// scale (×1.33 on a television), the way the zones get it through their CSS
// zoom; inside a zone (the bottom-left card, the follow strip) the zoom
// already applies the scale, so it reads 1 there. A surface the top bar
// covers is inset below it.

import { useLayoutEffect, useState, type RefObject } from "react";

import { useWidthClass } from "@/hooks/use-width-class";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import { gridMeasures } from "@/lib/cockpit-layout";
import { topBarInset } from "@/lib/minimap-geometry";

const ZONE = "[data-zone-rect]";
const TOP_BAR = '[data-zone-rect="top_bar"]';

export interface MapChrome {
  /** Multiply chrome px by this. */
  scale: number;
  /** CSS px to keep clear at the top (under the top bar). */
  insetTop: number;
  /** Inside a zone (the bottom-left card). */
  zoned: boolean;
}

export function useMapChrome(ref: RefObject<HTMLElement | null>): MapChrome {
  const { widthClass, portrait } = useWidthClass();
  const classScale = portrait ? 1 : gridMeasures(widthClass, false).scale;
  const [m, setM] = useState({ zoned: false, insetTop: 0 });

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const measure = () => {
      const zoned = el.closest(ZONE) !== null;
      const bar = zoned ? null : document.querySelector(TOP_BAR);
      const insetTop = topBarInset(el.getBoundingClientRect(), bar?.getBoundingClientRect() ?? null, CHROME_TOKENS.gap_px * classScale);
      setM((prev) => (prev.zoned === zoned && prev.insetTop === insetTop ? prev : { zoned, insetTop }));
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    window.addEventListener("resize", measure);
    return () => {
      ro.disconnect();
      window.removeEventListener("resize", measure);
    };
  }, [ref, classScale]);

  return { scale: m.zoned ? 1 : classScale, insetTop: m.insetTop, zoned: m.zoned };
}
