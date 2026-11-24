// What lies past the menu rail's visible part: the rail scrolls when its
// entries do not fit, and the clipped end shows a fade with a chevron.

/** What lies past the rail's visible part, along its scroll axis. */
export interface RailOverflow {
  axis: "x" | "y" | null;
  before: boolean;
  after: boolean;
}

export const NO_OVERFLOW: RailOverflow = { axis: null, before: false, after: false };

export function railOverflow(n: {
  scrollTop: number;
  scrollLeft: number;
  scrollHeight: number;
  scrollWidth: number;
  clientHeight: number;
  clientWidth: number;
}): RailOverflow {
  if (n.scrollHeight > n.clientHeight + 1) {
    return { axis: "y", before: n.scrollTop > 1, after: n.scrollTop + n.clientHeight < n.scrollHeight - 1 };
  }
  if (n.scrollWidth > n.clientWidth + 1) {
    return { axis: "x", before: n.scrollLeft > 1, after: n.scrollLeft + n.clientWidth < n.scrollWidth - 1 };
  }
  return NO_OVERFLOW;
}
