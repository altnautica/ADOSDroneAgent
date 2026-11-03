// Where a popup menu opens: under its anchor, flipped above it when there is
// little room below and more above, kept inside the window by a margin.

export const MENU_MARGIN_PX = 8;
export const MENU_MIN_WIDTH_PX = 200;
export const MENU_MAX_WIDTH_PX = 360;

export interface AnchorRect {
  left: number;
  top: number;
  bottom: number;
}

export interface MenuPlacement {
  left: number;
  /** Set when the menu opens below the anchor. */
  top?: number;
  /** Set when the menu flips above the anchor (distance from the bottom). */
  bottom?: number;
  maxHeight: number;
}

export function placeMenu(anchor: AnchorRect | null, viewportW: number, viewportH: number): MenuPlacement {
  const m = MENU_MARGIN_PX;
  const maxLeft = Math.max(m, viewportW - MENU_MIN_WIDTH_PX - m);
  if (!anchor) return { left: m, top: m, maxHeight: Math.max(0, viewportH - 2 * m) };
  const left = Math.min(maxLeft, Math.max(m, anchor.left));
  const below = viewportH - anchor.bottom - m;
  const above = anchor.top - m;
  const flip = below < viewportH / 3 && above > below;
  return flip
    ? { left, bottom: viewportH - anchor.top, maxHeight: Math.max(0, above) }
    : { left, top: anchor.bottom, maxHeight: Math.max(0, below) };
}
