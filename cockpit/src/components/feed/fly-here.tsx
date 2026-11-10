// "Fly here" on every map (the bottom-left card, the full map view, the split
// pane, the local frame): a long-press (`layout.json` `map_long_press_ms`)
// or a right-click picks a point. In the air, while the pilot accepts a
// surface `goto`, a sheet shows the point with its distance and a hold
// (`holds_ms.fly_here`) sends it; on the ground the same gesture opens the
// Launch sheet with the point as its go-to. Each map says how its points map
// to the world through `locate`.

import { useRef } from "react";

import { useFlyHere } from "@/hooks/use-fly-here";
import { enToLatLon, offsetFromDrone, unprojectEN } from "@/lib/fly-here";
import { latLonToEN, projectEN, type LatLon, type MapView } from "@/lib/minimap-geometry";
import { specText } from "@/shared/hud/spec-text";

/** The global minimap's Fly Here layer: an SVG over the map sharing its
 *  viewBox, so the picked marker sits where pressed. */
export function FlyHereLayer({ view, refPoint, drone, viewBox }: { view: MapView; refPoint: LatLon; drone: LatLon | null; viewBox: number }) {
  const svgRef = useRef<SVGSVGElement>(null);
  const fly = useFlyHere((clientX, clientY) => {
    const ctm = svgRef.current?.getScreenCTM();
    if (!ctm) return null;
    const p = new DOMPoint(clientX, clientY).matrixTransform(ctm.inverse());
    const en = unprojectEN(p.x, p.y, view);
    const ll = enToLatLon(en.e, en.n, refPoint);
    return { ll, fromDrone: offsetFromDrone(ll, drone) };
  }, drone);
  if (!fly.handlers && !fly.sheet) return null;

  const marker = fly.target ? latLonToEN(fly.target.lat, fly.target.lon, refPoint.lat, refPoint.lon) : null;
  const pt = marker ? projectEN(marker.e, marker.n, view) : null;
  return (
    <>
      <svg
        ref={svgRef}
        viewBox={`0 0 ${viewBox} ${viewBox}`}
        className="pointer-events-auto absolute inset-0 h-full w-full touch-none select-none"
        aria-label={specText("map.fly_here.long_press")}
        role="img"
        data-testid="fly-here-layer"
        {...fly.handlers}
      >
        {pt ? <FlyHereMarker x={pt.x} y={pt.y} r={3.2} /> : null}
      </svg>
      {fly.sheet}
    </>
  );
}

/** The picked point: a ring and a cross. */
export function FlyHereMarker({ x, y, r }: { x: number; y: number; r: number }) {
  return (
    <g className="text-hud-warn" stroke="currentColor" fill="none" strokeWidth={r * 0.375}>
      <circle cx={x} cy={y} r={r} />
      <line x1={x - r * 1.6} y1={y} x2={x + r * 1.6} y2={y} />
      <line x1={x} y1={y - r * 1.6} x2={x} y2={y + r * 1.6} />
    </g>
  );
}
