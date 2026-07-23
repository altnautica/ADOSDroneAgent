// Pure geometry for the flight minimap: turn the vehicle's lat/lon breadcrumb
// into local metres and fit those metres into a fixed square SVG. No basemap and
// no map library — the cockpit is lean, so this is a self-contained north-up
// local-tangent projection auto-scaled to the observed track.
//
// The vehicle telemetry gives lat/lon/heading only (no home datum, no tiles), so
// the map is drawn from what is genuinely reachable: the current position, the
// recent breadcrumb trail, and the session's first fix. Everything here is pure
// so the projection + fit math is unit-testable without a DOM.

/** WGS84 equatorial radius (m). Good enough for a local-tangent minimap that
 *  only ever spans metres to a few km. */
const EARTH_R = 6378137;
const DEG = Math.PI / 180;

export interface LatLon {
  lat: number;
  lon: number;
}

/** Metres east/north of a reference lat/lon (a local equirectangular tangent). */
export interface EN {
  e: number;
  n: number;
}

/** A point in SVG coordinates (origin top-left, +y down). */
export interface MapPoint {
  x: number;
  y: number;
}

/** The fitted map transform: metres → SVG for one render. `centerE`/`centerN`
 *  are the world metres that land on the SVG centre; `scale` is SVG units per
 *  metre. */
export interface MapView {
  scale: number;
  cx: number;
  cy: number;
  centerE: number;
  centerN: number;
}

/** Metres east/north of `(refLat, refLon)` via an equirectangular tangent —
 *  east scaled by cos(lat) so the aspect is faithful near the reference. */
export function latLonToEN(
  lat: number,
  lon: number,
  refLat: number,
  refLon: number,
): EN {
  const e = (lon - refLon) * DEG * EARTH_R * Math.cos(refLat * DEG);
  const n = (lat - refLat) * DEG * EARTH_R;
  return { e, n };
}

/** Ground distance in metres between two fixes (via the tangent around `a`). */
export function distanceMeters(a: LatLon, b: LatLon): number {
  const { e, n } = latLonToEN(b.lat, b.lon, a.lat, a.lon);
  return Math.hypot(e, n);
}

/** A lat/lon that is finite, in range, and not the null-island (0,0) sentinel a
 *  fix-less vehicle reports. */
export function isValidFix(lat: unknown, lon: unknown): boolean {
  if (typeof lat !== "number" || typeof lon !== "number") return false;
  if (!Number.isFinite(lat) || !Number.isFinite(lon)) return false;
  if (lat < -90 || lat > 90 || lon < -180 || lon > 180) return false;
  // Exactly (0,0) is the no-fix sentinel, not a real position on open ocean.
  return !(lat === 0 && lon === 0);
}

/**
 * Fit `points` (metres east/north) into a `view × view` SVG with `pad` on each
 * edge, never zooming past `minSpanM` (so a stationary vehicle shows a sensible
 * window instead of an infinite zoom). Returns `null` for an empty set (nothing
 * to draw). The fit is square + centred so north-up stays north-up.
 */
export function computeMapView(
  points: EN[],
  view: number,
  pad: number,
  minSpanM: number,
): MapView | null {
  if (points.length === 0) return null;
  let minE = Infinity;
  let maxE = -Infinity;
  let minN = Infinity;
  let maxN = -Infinity;
  for (const p of points) {
    if (p.e < minE) minE = p.e;
    if (p.e > maxE) maxE = p.e;
    if (p.n < minN) minN = p.n;
    if (p.n > maxN) maxN = p.n;
  }
  const centerE = (minE + maxE) / 2;
  const centerN = (minN + maxN) / 2;
  const span = Math.max(maxE - minE, maxN - minN, minSpanM);
  const scale = (view - 2 * pad) / span;
  return { scale, cx: view / 2, cy: view / 2, centerE, centerN };
}

/** Project a world point (metres east/north) to SVG coords. North-up: +north
 *  is up (smaller y). */
export function projectEN(e: number, n: number, v: MapView): MapPoint {
  return {
    x: v.cx + (e - v.centerE) * v.scale,
    y: v.cy - (n - v.centerN) * v.scale,
  };
}

/** Metres spanned across the full SVG width at this scale — the map's scale
 *  readout. */
export function viewSpanMeters(v: MapView, view: number): number {
  return view / v.scale;
}
