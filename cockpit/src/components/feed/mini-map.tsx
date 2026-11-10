// A lean flight minimap over the feed — no basemap, no map library. It draws
// what the vehicle telemetry genuinely provides: the current position (a
// heading-rotated marker), the recent breadcrumb trail, and the session's first
// fix ("start"). North-up, auto-scaled to the observed track. Purely
// informational (pointer-events pass through) and reduced-motion-safe (no
// animation — the marker's position/rotation are data-driven, not decorative
// motion).
//
// Honest surfaces: there is no home datum on the wire, so the map never
// draws one — the "start" marker is the first fix we observed this session (our
// launch proxy), labelled as such, not the flight controller's home. Without a
// GNSS fix the drone and "Fly here" still use the FC's position through its EKF
// origin (marked "no fix"); with neither it shows a plain "No GPS fix" state;
// when the fix goes stale it dims the last known track rather than pinning a
// live-looking marker.
//
// Without GNSS the minimap switches to the local frame (see
// `local-mini-map`): AUTO goes local after 2 s below a 3D fix or while the
// active EKF horizontal source is ExternalNav or None; the operator can pin
// LOCAL or MAP from the header (persisted). The header also opens the Map
// panel while a mapping session reports.

import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { Layers } from "lucide-react";

import { FlyHereLayer } from "@/components/feed/fly-here";
import { flyHerePosition } from "@/lib/fly-here";
import { LocalMiniMap } from "@/components/feed/local-mini-map";
import { useElementSize } from "@/hooks/use-element-size";
import { useMapChrome } from "@/hooks/use-map-chrome";
import { useNow } from "@/hooks/use-now";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import { minimapFrame, nextMinimapPin } from "@/lib/local-minimap";
import { nextNoFixSince } from "@/shared/hud/minimap-mode";
import { MiniMapReadoutRow, type MiniMapReadout } from "@/components/feed/minimap-readout";
import { useNavStreamStore } from "@/stores/nav-stream-store";
import { useSettingsStore } from "@/stores/settings-store";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusStore } from "@/stores/status-store";
import {
  computeMapView,
  latLonToEN,
  projectEN,
  svgTextUnits,
  viewSpanMeters,
  type EN,
  type LatLon,
} from "@/lib/minimap-geometry";
import { useTrackStore } from "@/stores/track-store";
import { cn } from "@/lib/utils";
import { useMissionRoute } from "@/lib/mission-route";
import { specText } from "@/shared/hud/spec-text";
import { setupText } from "@/shared/setup-wizard/strings";
import tokens from "@/shared/cockpit-spec/tokens.json";

const VIEW = 100;
const PAD = 12;
/** Minimum window (m) so a stationary vehicle shows a sensible area. */
const MIN_SPAN_M = 40;
const MISSION_COLOR = tokens.state.mission;

/** A heading value that is a real compass bearing (guards the MAVLink 65535
 *  "unknown" sentinel that divides to 655.35). */
function headingOrNull(h: number | null | undefined): number | null {
  return h != null && Number.isFinite(h) && h >= 0 && h <= 360 ? h : null;
}

const MapPanel = lazy(() => import("@/components/feed/map-panel"));

const PIN_WORD = { auto: "AUTO", local: "LOCAL", map: "MAP" } as const;

export function MiniMap({ className }: { className?: string } = {}) {
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const nav = useNavStreamStore((s) => s.nav);
  const lastCommandedSet = useNavStreamStore((s) => s.lastCommandedSet);
  const ekfParams = useNavStreamStore((s) => s.ekfParams);
  const hasMapStatus = useNavStreamStore((s) => s.map !== null);
  const pin = useSettingsStore((s) => s.minimapPin);
  const setPin = useSettingsStore((s) => s.setMinimapPin);
  const now = useNow(500);
  const [panelOpen, setPanelOpen] = useState(false);

  // The start of the current below-3D run, updated as the fix changes.
  const fix = live ? (telemetry?.gps?.fix_type ?? null) : null;
  const [noFix, setNoFix] = useState<{ fix: number | null; since: number | null }>({ fix: null, since: null });
  if (noFix.fix !== fix) setNoFix({ fix, since: nextNoFixSince(noFix.since, fix, now) });

  const { frame, reason } = minimapFrame({
    pin,
    noFixSince: noFix.since,
    now,
    ekfParams,
    activeSet: nav?.data.activeSet ?? lastCommandedSet,
  });
  const local = frame === "local";
  const rootRef = useRef<HTMLDivElement>(null);
  const { scale, insetTop, zoned } = useMapChrome(rootRef);
  const labelPx = CHROME_TOKENS.type.label_px * scale;

  return (
    <div ref={rootRef} className={cn("relative h-[96px] w-[96px] shrink-0", className)} data-testid="minimap" data-inset-top={insetTop}>
      <div
        className={cn(
          "relative h-full w-full overflow-hidden rounded-lg border border-surface-foreground/15 bg-background/55 backdrop-blur-sm",
          !live && "opacity-70",
        )}
      >
        <div className="absolute inset-x-0 bottom-0" style={{ top: insetTop }}>
          {local ? <LocalMiniMap scale={scale} inCard={zoned} /> : <GlobalMiniMap scale={scale} inCard={zoned} />}
          <div
            // The card's frame draws its title top-left, so the card keeps
            // these top-right; a full map keeps them top-left.
            className={cn("pointer-events-none absolute inset-x-0 top-0 z-10 flex items-start gap-[4px] px-[4px]", zoned ? "justify-end" : "justify-start")}
            style={{ fontSize: labelPx }}
            data-testid="minimap-header"
          >
            <span className="pt-[2px] uppercase tracking-wide text-muted-foreground">
              {local ? (reason === "pinned" ? "Local" : "Local · no GPS") : specText("cockpit.card.map")}
            </span>
            <span className="flex gap-[2px]">
              {hasMapStatus ? (
                <button
                  type="button"
                  onClick={() => setPanelOpen(true)}
                  aria-haspopup="dialog"
                  aria-label="Mapping status"
                  className="pointer-events-auto flex min-h-[24px] items-center rounded px-[4px] text-hud-ink hover:bg-hud-hair"
                >
                  <Layers style={{ width: labelPx * 1.2, height: labelPx * 1.2 }} aria-hidden />
                </button>
              ) : null}
              <button
                type="button"
                onClick={() => setPin(nextMinimapPin(pin))}
                aria-label={`Minimap frame ${PIN_WORD[pin]}; change`}
                className="pointer-events-auto min-h-[24px] rounded px-[4px] font-mono text-surface-foreground/80 hover:bg-hud-hair"
              >
                {PIN_WORD[pin]}
              </button>
            </span>
          </div>
        </div>
      </div>
      {panelOpen ? (
        <Suspense fallback={null}>
          <MapPanel onClose={() => setPanelOpen(false)} />
        </Suspense>
      ) : null}
    </div>
  );
}

function GlobalMiniMap({ scale, inCard }: { scale: number; inCard: boolean }) {
  const boxRef = useRef<HTMLDivElement>(null);
  const box = useElementSize(boxRef);
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const status = useStatusStore((s) => s.status);
  const start = useTrackStore((s) => s.start);
  const samples = useTrackStore((s) => s.samples);
  const record = useTrackStore((s) => s.record);
  const syncVehicle = useTrackStore((s) => s.syncVehicle);

  // Reset the breadcrumb when the paired vehicle's identity changes (a
  // mid-session re-pair on a ground station), so the trail + "start" never span
  // two vehicles. The store ignores a null id (a transient drop) and adopts the
  // first identity without clearing, so this is a no-op on a drone's own track.
  const pairedDeviceId = status?.paired_drone?.device_id ?? null;
  useEffect(() => {
    syncVehicle(pairedDeviceId);
  }, [pairedDeviceId, syncVehicle]);

  const pos = telemetry?.position ?? null;
  // The drone: a GNSS fix, else the FC's position through its EKF origin (as
  // Mission Control places it). The breadcrumb records only a GNSS fix; the
  // marker and "Fly here" use either.
  const { drone: dronePos, gnss: hasFix } = flyHerePosition(pos, telemetry?.gps?.fix_type ?? null, live);
  const droneFix: LatLon | null = hasFix ? dronePos : null;
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
  const ref: LatLon | null = start ?? dronePos ?? samples[0] ?? null;

  const trailEN: EN[] = ref
    ? samples.map((s) => latLonToEN(s.lat, s.lon, ref.lat, ref.lon))
    : [];
  const droneEN: EN | null =
    ref && dronePos ? latLonToEN(dronePos.lat, dronePos.lon, ref.lat, ref.lon) : null;
  const startEN: EN | null =
    ref && start ? latLonToEN(start.lat, start.lon, ref.lat, ref.lon) : null;

  const route = useMissionRoute();
  const routeEN: EN[] = ref ? route.map((p) => latLonToEN(p.lat, p.lon, ref.lat, ref.lon)) : [];
  const fitPoints: EN[] = [...trailEN, ...routeEN, ...(droneEN ? [droneEN] : [])];
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

  // Map text keeps its chrome size whatever the map's size.
  const textUnits = svgTextUnits(CHROME_TOKENS.type.label_px * scale, Math.min(box.w, box.h), VIEW);
  const readout: MiniMapReadout | null = empty
    ? null
    : { scale: spanM >= 1000 ? `~${(spanM / 1000).toFixed(1)} km` : `~${Math.round(spanM)} m`, warn: hasFix ? null : specText("alert.gps_no_3d.no_fix") };

  return (
    <div ref={boxRef} className="pointer-events-none relative h-full w-full" aria-hidden>
      <div className="relative h-full w-full">
        {empty ? (
          <div
            className="flex h-full w-full items-center justify-center px-[4px] text-center text-muted-foreground"
            style={{ fontSize: CHROME_TOKENS.type.label_px * scale }}
          >
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

              {/* the mission on the drone (the router's mirror) */}
              {view && routeEN.length > 0 ? (
                <g stroke={MISSION_COLOR} fill={MISSION_COLOR}>
                  <title>{setupText("map.mission")}</title>
                  {routeEN.length >= 2 ? (
                    <polyline
                      points={routeEN.map((p) => {
                        const s = projectEN(p.e, p.n, view);
                        return `${s.x.toFixed(1)},${s.y.toFixed(1)}`;
                      }).join(" ")}
                      fill="none"
                      strokeWidth={1}
                      strokeDasharray="3 2"
                    />
                  ) : null}
                  {routeEN.map((p, i) => {
                    const s = projectEN(p.e, p.n, view);
                    return <circle key={route[i].seq} cx={s.x} cy={s.y} r={1.6} stroke="none" />;
                  })}
                </g>
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
                    x={startPt.x + textUnits * 0.6}
                    y={startPt.y + textUnits * 0.4}
                    fontSize={textUnits}
                    fontFamily="monospace"
                  >
                    S
                  </text>
                </g>
              ) : null}

              {/* the vehicle — a heading-rotated arrow, or a dot when heading is
                  unknown */}
              {dronePt ? (
                <g className="text-hud-primary" fill="currentColor" stroke="none">
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
                  y={textUnits * 1.2}
                  textAnchor="middle"
                  fontSize={textUnits}
                  fill="currentColor"
                  fontFamily="monospace"
                  data-testid="minimap-north"
                >
                  N
                </text>
              </g>
            </svg>
            {view && ref && dronePos ? <FlyHereLayer view={view} refPoint={ref} drone={dronePos} viewBox={VIEW} /> : null}
          </>
        )}
        <MiniMapReadoutRow readout={readout} scale={scale} inCard={inCard} />
      </div>
    </div>
  );
}
