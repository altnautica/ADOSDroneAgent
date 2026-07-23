// A lean flight minimap over the feed — no basemap, no map library. It draws
// what the vehicle telemetry genuinely provides: the current position (a
// heading-rotated marker), the recent breadcrumb trail, and the session's first
// fix ("start"). North-up, auto-scaled to the observed track. Purely
// informational (pointer-events pass through) and reduced-motion-safe (no
// animation — the marker's position/rotation are data-driven, not decorative
// motion).
//
// Honest surfaces (Rule 44): there is no home datum on the wire, so the map never
// draws one — the "start" marker is the first fix we observed this session (our
// launch proxy), labelled as such, not the flight controller's home. With no
// valid fix it shows a plain "No GPS fix" state; when the fix goes stale it dims
// the last known track rather than pinning a live-looking marker.

import { useEffect } from "react";

import { useFlightTelemetryContext } from "@/hooks/flight-telemetry-context";
import {
  computeMapView,
  isValidFix,
  latLonToEN,
  projectEN,
  viewSpanMeters,
  type EN,
  type LatLon,
} from "@/lib/minimap-geometry";
import { useTrackStore } from "@/stores/track-store";
import { cn } from "@/lib/utils";

const VIEW = 100;
const PAD = 12;
/** Minimum window (m) so a stationary vehicle shows a sensible area. */
const MIN_SPAN_M = 40;

/** A heading value that is a real compass bearing (guards the MAVLink 65535
 *  "unknown" sentinel that divides to 655.35). */
function headingOrNull(h: number | null | undefined): number | null {
  return h != null && Number.isFinite(h) && h >= 0 && h <= 360 ? h : null;
}

export function MiniMap() {
  const { telemetry, live } = useFlightTelemetryContext();
  const start = useTrackStore((s) => s.start);
  const samples = useTrackStore((s) => s.samples);
  const record = useTrackStore((s) => s.record);

  const pos = telemetry?.position ?? null;
  const fix = telemetry?.gps?.fix_type ?? null;
  // Narrow the nullable wire coords to a real LatLon (or null) once.
  const coords: LatLon | null =
    pos != null &&
    typeof pos.lat === "number" &&
    typeof pos.lon === "number" &&
    isValidFix(pos.lat, pos.lon)
      ? { lat: pos.lat, lon: pos.lon }
      : null;
  // A null fix-type (some republished GS snapshots) falls back to coord validity;
  // otherwise require at least a 2D fix so a garbage first sample is not plotted.
  const fixOk = coords != null && (fix == null || fix >= 2);
  const hasFix = live && fixOk;
  const droneFix: LatLon | null = hasFix ? coords : null;
  const headingDeg = headingOrNull(pos?.heading);

  // Record the live fix into the breadcrumb (the store applies the jitter filter
  // + bound). Runs only on a real, live fix so a stale/absent position never
  // extends the trail. Depend on the primitives so it does not fire every render.
  const droneLat = droneFix?.lat ?? null;
  const droneLon = droneFix?.lon ?? null;
  useEffect(() => {
    if (droneLat != null && droneLon != null) record(droneLat, droneLon);
  }, [droneLat, droneLon, record]);

  // Reference for the local projection: the start, else the live fix, else the
  // first breadcrumb. Without any of these there is nothing to draw.
  const ref: LatLon | null = start ?? droneFix ?? samples[0] ?? null;

  const trailEN: EN[] = ref
    ? samples.map((s) => latLonToEN(s.lat, s.lon, ref.lat, ref.lon))
    : [];
  const droneEN: EN | null =
    ref && droneFix ? latLonToEN(droneFix.lat, droneFix.lon, ref.lat, ref.lon) : null;
  const startEN: EN | null =
    ref && start ? latLonToEN(start.lat, start.lon, ref.lat, ref.lon) : null;

  const fitPoints: EN[] = [...trailEN, ...(droneEN ? [droneEN] : [])];
  const view = computeMapView(fitPoints, VIEW, PAD, MIN_SPAN_M);

  const empty = view == null;
  const spanM = view ? viewSpanMeters(view, VIEW) : 0;

  const trailPts =
    view && trailEN.length >= 2
      ? trailEN
          .map((p) => {
            const s = projectEN(p.e, p.n, view);
            return `${s.x.toFixed(1)},${s.y.toFixed(1)}`;
          })
          .join(" ")
      : null;

  const startPt = view && startEN ? projectEN(startEN.e, startEN.n, view) : null;
  const dronePt = view && droneEN ? projectEN(droneEN.e, droneEN.n, view) : null;

  return (
    <div
      className="pointer-events-none absolute bottom-[3.6rem] right-[0.6rem] z-[8] h-[5.6rem] w-[5.6rem]"
      aria-hidden
    >
      <div
        className={cn(
          "relative h-full w-full overflow-hidden rounded-lg border border-surface-foreground/15 bg-background/55 backdrop-blur-sm",
          !live && "opacity-70",
        )}
      >
        {/* top-left label */}
        <span className="absolute left-[0.25rem] top-[0.1rem] z-10 text-[0.5rem] uppercase tracking-wide text-muted-foreground">
          Map
        </span>

        {empty ? (
          <div className="flex h-full w-full items-center justify-center px-[0.3rem] text-center text-[0.55rem] text-muted-foreground">
            No GPS fix
          </div>
        ) : (
          <>
            <svg
              viewBox={`0 0 ${VIEW} ${VIEW}`}
              className="h-full w-full"
              style={{ filter: "drop-shadow(0 0 1px rgba(0,0,0,0.9))" }}
            >
              {/* faint centre hairlines for orientation */}
              <g stroke="currentColor" className="text-surface-foreground/15">
                <line x1={VIEW / 2} y1={6} x2={VIEW / 2} y2={VIEW - 6} />
                <line x1={6} y1={VIEW / 2} x2={VIEW - 6} y2={VIEW / 2} />
              </g>

              {/* breadcrumb trail */}
              {trailPts ? (
                <polyline
                  points={trailPts}
                  fill="none"
                  stroke="currentColor"
                  className="text-surface-foreground/45"
                  strokeWidth={1.4}
                  strokeLinejoin="round"
                  strokeLinecap="round"
                />
              ) : null}

              {/* start marker (session first fix — launch proxy, not FC home) */}
              {startPt ? (
                <g
                  className="text-ok"
                  fill="currentColor"
                  stroke="none"
                >
                  <circle cx={startPt.x} cy={startPt.y} r={2.4} />
                  <text
                    x={startPt.x + 3.5}
                    y={startPt.y + 2.5}
                    fontSize={6}
                    fontFamily="monospace"
                  >
                    S
                  </text>
                </g>
              ) : null}

              {/* the vehicle — a heading-rotated arrow, or a dot when heading is
                  unknown */}
              {dronePt ? (
                <g className="text-amber" fill="currentColor" stroke="none">
                  {headingDeg != null ? (
                    <polygon
                      points="0,-4.6 3.2,4 0,2 -3.2,4"
                      transform={`translate(${dronePt.x} ${dronePt.y}) rotate(${headingDeg})`}
                    />
                  ) : (
                    <circle cx={dronePt.x} cy={dronePt.y} r={2.8} />
                  )}
                </g>
              ) : null}

              {/* north indicator (north-up) */}
              <g className="text-muted-foreground">
                <text
                  x={VIEW / 2}
                  y={8}
                  textAnchor="middle"
                  fontSize={6}
                  fill="currentColor"
                  fontFamily="monospace"
                >
                  N
                </text>
              </g>
            </svg>

            {/* scale + stale readout */}
            <div className="absolute bottom-[0.1rem] left-0 right-0 flex items-center justify-between px-[0.3rem]">
              <span className="font-mono text-[0.5rem] text-muted-foreground">
                {spanM >= 1000
                  ? `~${(spanM / 1000).toFixed(1)} km`
                  : `~${Math.round(spanM)} m`}
              </span>
              {!hasFix ? (
                <span className="font-mono text-[0.5rem] text-warn">no fix</span>
              ) : null}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
