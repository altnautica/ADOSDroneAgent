// Geometry for the controller drawing in Settings › Controls: the callout
// labels sit in two fixed columns beside the art (never over it, never over
// each other, never past the edge), and each drawn control gets an invisible
// hit area large enough to tap.

export type CalloutSide = "left" | "right";

export interface CalloutInput {
  control: string;
  label: string;
  /** The spec's preferred side; "top"/"bottom" fall to the nearer column. */
  labelSide: string;
  anchor: readonly [number, number];
}

export interface Callout {
  control: string;
  label: string;
  side: CalloutSide;
  /** Baseline-centre y of the label row in viewBox units. */
  y: number;
}

/** Width of each label column beside the art, in viewBox units. */
export const CALLOUT_COLUMN = 280;
/** Vertical pitch between two label rows (the label font is 18 units). */
export const CALLOUT_PITCH = 26;
/** Longest label kept whole; longer ones end in an ellipsis (full text in a tooltip). */
export const CALLOUT_MAX_CHARS = 24;

export function fitLabel(label: string, max = CALLOUT_MAX_CHARS): string {
  return label.length > max ? `${label.slice(0, max - 1).trimEnd()}…` : label;
}

/** Bounding-box centre and half short side of an absolute M/L/H/V path. */
export function pathBox(d: string): { centre: [number, number]; radius: number } {
  const xs: number[] = [];
  const ys: number[] = [];
  let x = 0;
  let y = 0;
  for (const [, cmd, args] of d.matchAll(/([MLHVZ])([^MLHVZ]*)/gi)) {
    const n = args.trim().split(/[\s,]+/).filter(Boolean).map(Number);
    const c = cmd.toUpperCase();
    if (c === "H") n.forEach((v) => { x = v; xs.push(x); ys.push(y); });
    else if (c === "V") n.forEach((v) => { y = v; xs.push(x); ys.push(y); });
    else if (c === "M" || c === "L") for (let i = 0; i + 1 < n.length; i += 2) { x = n[i]; y = n[i + 1]; xs.push(x); ys.push(y); }
  }
  if (!xs.length) return { centre: [0, 0], radius: 0 };
  const [x0, x1, y0, y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
  return { centre: [(x0 + x1) / 2, (y0 + y1) / 2], radius: Math.min(x1 - x0, y1 - y0) / 2 };
}

/** Place every callout in its column, in anchor order, at least one pitch
 *  apart and inside [top, bottom]. */
export function layoutCallouts(
  items: readonly CalloutInput[],
  view: { x: number; y: number; w: number; h: number },
  pitch = CALLOUT_PITCH,
): Callout[] {
  const mid = view.x + view.w / 2;
  const top = view.y + pitch / 2;
  const bottom = view.y + view.h - pitch / 2;
  const out: Callout[] = [];
  for (const side of ["left", "right"] as const) {
    const col = items
      .filter((i) => (i.labelSide === "left" || i.labelSide === "right" ? i.labelSide : i.anchor[0] < mid ? "left" : "right") === side)
      .sort((a, b) => a.anchor[1] - b.anchor[1] || a.anchor[0] - b.anchor[0]);
    const ys = col.map((i) => i.anchor[1]);
    for (let k = 0; k < ys.length; k++) ys[k] = Math.max(ys[k], top, k ? ys[k - 1] + pitch : -Infinity);
    for (let k = ys.length - 1; k >= 0; k--) ys[k] = Math.min(ys[k], k === ys.length - 1 ? bottom : ys[k + 1] - pitch);
    col.forEach((i, k) => out.push({ control: i.control, label: i.label, side, y: ys[k] }));
  }
  return out;
}

/** Hit radius for each control: the wanted radius, but never past half the
 *  distance to the nearest other control (so neighbours keep their own
 *  taps), and never smaller than the drawn control. */
export function hitRadii(
  centres: Readonly<Record<string, readonly [number, number]>>,
  drawn: Readonly<Record<string, number>>,
  wanted: number,
): Record<string, number> {
  const ids = Object.keys(centres);
  const out: Record<string, number> = {};
  for (const id of ids) {
    const [x, y] = centres[id];
    let nearest = Infinity;
    for (const other of ids) {
      if (other === id) continue;
      const [ox, oy] = centres[other];
      const d = Math.hypot(ox - x, oy - y);
      if (d > 0) nearest = Math.min(nearest, d);
    }
    out[id] = Math.max(drawn[id] ?? 0, Math.min(wanted, nearest / 2));
  }
  return out;
}
