// "Fly here" on the global minimap: a long-press picks a point, a sheet shows
// it with its distance, and a hold sends the pilot's `goto`. Rendered only
// while the pilot lists `goto` in `surface_ops` and the drone is flying; the
// layer shares the minimap's viewBox so the picked marker sits where pressed.

import { useRef, useState, type PointerEvent } from "react";

import { Sheet } from "@/components/ui/sheet";
import { HoldButton } from "@/components/ui/hold-button";
import { usePilot } from "@/hooks/use-pilot";
import {
  FLY_HERE_PRESS_MS,
  FLY_HERE_SLOP_PX,
  enToLatLon,
  flyHereArgs,
  flyHereOffered,
  flyHereRefusal,
  unprojectEN,
} from "@/lib/fly-here";
import { distanceMeters, latLonToEN, projectEN, type LatLon, type MapView } from "@/lib/minimap-geometry";
import { sendPilotAction } from "@/lib/pilot-link";
import { useFlightStore } from "@/stores/flight-store";

export function FlyHereLayer({ view, refPoint, drone, viewBox }: { view: MapView; refPoint: LatLon; drone: LatLon | null; viewBox: number }) {
  const pilot = usePilot();
  const altRel = useFlightStore((s) => s.telemetry?.position?.alt_rel ?? null);
  const svgRef = useRef<SVGSVGElement>(null);
  const press = useRef<{ x: number; y: number; timer: number } | null>(null);
  const [target, setTarget] = useState<LatLon | null>(null);
  const [result, setResult] = useState<string | null>(null);
  const [sending, setSending] = useState(false);

  if (!flyHereOffered(pilot?.data ?? null)) return null;

  const cancel = () => {
    if (press.current) window.clearTimeout(press.current.timer);
    press.current = null;
  };
  const onDown = (e: PointerEvent<SVGSVGElement>) => {
    cancel();
    const { clientX, clientY } = e;
    press.current = {
      x: clientX,
      y: clientY,
      timer: window.setTimeout(() => {
        press.current = null;
        const ctm = svgRef.current?.getScreenCTM();
        if (!ctm) return;
        const p = new DOMPoint(clientX, clientY).matrixTransform(ctm.inverse());
        const en = unprojectEN(p.x, p.y, view);
        setResult(null);
        setTarget(enToLatLon(en.e, en.n, refPoint));
      }, FLY_HERE_PRESS_MS),
    };
  };
  const onMove = (e: PointerEvent<SVGSVGElement>) => {
    const p = press.current;
    if (p && Math.hypot(e.clientX - p.x, e.clientY - p.y) > FLY_HERE_SLOP_PX) cancel();
  };

  const marker = target ? latLonToEN(target.lat, target.lon, refPoint.lat, refPoint.lon) : null;
  const pt = marker ? projectEN(marker.e, marker.n, view) : null;
  const dist = target && drone ? distanceMeters(drone, target) : null;

  const fly = () => {
    if (!target) return;
    setSending(true);
    void sendPilotAction("goto", flyHereArgs(target, altRel)).then((r) => {
      setSending(false);
      if (r.ok) setTarget(null);
      else setResult(flyHereRefusal(r.err));
    });
  };

  return (
    <>
      <svg
        ref={svgRef}
        viewBox={`0 0 ${viewBox} ${viewBox}`}
        className="pointer-events-auto absolute inset-0 h-full w-full touch-none select-none"
        aria-label="Long-press the map to fly there"
        role="img"
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={cancel}
        onPointerCancel={cancel}
        onPointerLeave={cancel}
        onContextMenu={(e) => e.preventDefault()}
      >
        {pt ? (
          <g className="text-hud-warn" stroke="currentColor" fill="none" strokeWidth={1.2}>
            <circle cx={pt.x} cy={pt.y} r={3.2} />
            <line x1={pt.x - 5} y1={pt.y} x2={pt.x + 5} y2={pt.y} />
            <line x1={pt.x} y1={pt.y - 5} x2={pt.x} y2={pt.y + 5} />
          </g>
        ) : null}
      </svg>
      {target ? (
        <Sheet
          title="Fly here"
          onDismiss={() => setTarget(null)}
          footer={
            <HoldButton onFired={fly} disabled={sending} className="w-full" label="Hold to fly here">
              Hold to fly here
            </HoldButton>
          }
        >
          <dl className="grid grid-cols-[auto_1fr] gap-x-[1rem] gap-y-[0.45rem] text-[0.85rem]">
            <dt className="text-muted-foreground">Point</dt>
            <dd className="font-mono">
              {target.lat.toFixed(6)}, {target.lon.toFixed(6)}
            </dd>
            <dt className="text-muted-foreground">Distance</dt>
            <dd className="font-mono">{dist === null ? "—" : `${Math.round(dist)} m`}</dd>
            <dt className="text-muted-foreground">Height</dt>
            <dd className="font-mono">{flyHereArgs(target, altRel).alt_m} m above home</dd>
          </dl>
          {result ? <p className="mt-[0.6rem] text-[0.85rem] text-hud-crit">{result}</p> : null}
        </Sheet>
      ) : null}
    </>
  );
}
