// A drone-centric radar over the feed. Its live core is what the telemetry
// genuinely provides: the vehicle's heading (a centre nose arrow) and its
// course-over-ground (an outer-ring marker from the velocity vector), on a
// north-up compass. It additionally paints surround-obstacle sectors, but ONLY
// from real OBSTACLE_DISTANCE data when the agent's sensor stack surfaces a
// proximity sensor onto the snapshot — the base wire carries none today, so the
// obstacle overlay simply does not appear (it never draws a false "all clear",
// because this is a heading instrument, not an obstacle-only radar).
//
// Purely informational (pointer-events pass through) and reduced-motion-safe (no
// animation — the arrow/marker are data-driven, not decorative motion). Honest
// surfaces (Rule 44): with no live heading, track, or obstacle it shows the
// compass frame + "no heading" rather than a fabricated pose.

import { useFlightTelemetryContext } from "@/hooks/flight-telemetry-context";
import {
  bearingToXY,
  obstacleSectors,
  sectorArcPath,
  trackBearing,
} from "@/lib/radar-geometry";
import { cn } from "@/lib/utils";

const CX = 50;
const CY = 50;
const OUTER_R = 42;
const MID_R = 30;
const INNER_R = 13;

/** A heading value that is a real compass bearing (guards the MAVLink 65535
 *  sentinel that divides to 655.35). */
function headingOrNull(h: number | null | undefined): number | null {
  return h != null && Number.isFinite(h) && h >= 0 && h <= 360 ? h : null;
}

const CARDINALS: { label: string; bearing: number }[] = [
  { label: "N", bearing: 0 },
  { label: "E", bearing: 90 },
  { label: "S", bearing: 180 },
  { label: "W", bearing: 270 },
];

export function ProximityRadar() {
  const { telemetry, live } = useFlightTelemetryContext();

  const pos = telemetry?.position;
  const vel = telemetry?.velocity;
  const obs = live ? telemetry?.obstacle : null;

  const heading = live ? headingOrNull(pos?.heading) : null;
  const track = live ? trackBearing(vel?.vx, vel?.vy) : null;
  const field = obstacleSectors(
    obs?.distances_cm,
    obs?.increment_deg ?? 5,
    obs?.angle_offset_deg ?? 0,
  );

  const nearestM = field.closestCm != null ? field.closestCm / 100 : null;
  const trackPt = track != null ? bearingToXY(track, OUTER_R - 3, CX, CY) : null;

  // The bottom readout: nearest obstacle takes priority, else the heading value,
  // else the honest empty state. Never a fabricated "clear".
  const bottom =
    nearestM != null
      ? {
          text: `${nearestM.toFixed(1)} m`,
          cls: nearestM < 2 ? "text-err" : "text-warn",
        }
      : heading != null
        ? {
            text: `${String(Math.round(heading) % 360).padStart(3, "0")}°`,
            cls: "text-surface-foreground/80",
          }
        : { text: "no heading", cls: "text-muted-foreground" };

  return (
    <div
      className="pointer-events-none absolute bottom-[3.6rem] right-[6.8rem] z-[8] h-[5.6rem] w-[5.6rem]"
      aria-hidden
    >
      <div
        className={cn(
          "relative h-full w-full overflow-hidden rounded-lg border border-surface-foreground/15 bg-background/55 backdrop-blur-sm",
          !live && "opacity-70",
        )}
      >
        <span className="absolute left-[0.25rem] top-[0.1rem] z-10 text-[0.5rem] uppercase tracking-wide text-muted-foreground">
          Radar
        </span>

        <svg
          viewBox="0 0 100 100"
          className="h-full w-full"
          style={{ filter: "drop-shadow(0 0 1px rgba(0,0,0,0.9))" }}
        >
          {/* range rings + cardinal cross */}
          <g fill="none" stroke="currentColor" className="text-surface-foreground/15">
            <circle cx={CX} cy={CY} r={OUTER_R} />
            <circle cx={CX} cy={CY} r={MID_R} />
            <circle cx={CX} cy={CY} r={INNER_R} />
            <line x1={CX} y1={CY - OUTER_R} x2={CX} y2={CY + OUTER_R} />
            <line x1={CX - OUTER_R} y1={CY} x2={CX + OUTER_R} y2={CY} />
          </g>

          {/* obstacle sectors (only when real ranges are present) */}
          {field.sectors.map((s, i) => (
            <path
              key={i}
              d={sectorArcPath(s.startDeg, s.endDeg, INNER_R, OUTER_R, CX, CY)}
              className={s.severity === "danger" ? "text-err" : "text-warn"}
              fill="currentColor"
              fillOpacity={s.severity === "danger" ? 0.32 : 0.26}
              stroke="currentColor"
              strokeWidth={0.8}
            />
          ))}

          {/* cardinal labels */}
          <g className="text-muted-foreground" fill="currentColor" fontFamily="monospace">
            {CARDINALS.map((c) => {
              const p = bearingToXY(c.bearing, OUTER_R + 4, CX, CY);
              return (
                <text
                  key={c.label}
                  x={p.x}
                  y={p.y + 2}
                  textAnchor="middle"
                  fontSize={6}
                >
                  {c.label}
                </text>
              );
            })}
          </g>

          {/* course-over-ground marker on the outer ring */}
          {trackPt ? (
            <g className="text-ok" fill="currentColor" stroke="none">
              <polygon
                points="0,-4 3,3 -3,3"
                transform={`translate(${trackPt.x} ${trackPt.y}) rotate(${track})`}
              />
            </g>
          ) : null}

          {/* the vehicle nose — heading arrow, or a plain dot when heading is
              unknown */}
          <g className="text-amber" fill="currentColor" stroke="none">
            {heading != null ? (
              <polygon
                points="0,-11 5.5,6 0,3 -5.5,6"
                transform={`translate(${CX} ${CY}) rotate(${heading})`}
              />
            ) : (
              <circle cx={CX} cy={CY} r={3} />
            )}
          </g>
        </svg>

        {/* bottom readout: nearest obstacle / heading / honest empty state */}
        <div className="absolute bottom-[0.1rem] left-0 right-0 text-center">
          <span className={cn("font-mono text-[0.5rem]", bottom.cls)}>
            {bottom.text}
          </span>
        </div>
      </div>
    </div>
  );
}
