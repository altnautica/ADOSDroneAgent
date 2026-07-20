// The artificial horizon — an inside-out attitude indicator drawn as line-art
// over the live video, so the feed shows through (no opaque sky/ground fill).
// A fixed boresight marks the aircraft; the horizon line + pitch ladder rotate
// with bank and translate with pitch; a fixed roll scale with a moving pointer
// reads bank. Attitude comes from the shared flight telemetry (radians, so it
// is converted to degrees here). When the link is not live it draws a dimmed
// "no attitude" state instead of a fabricated level horizon.
//
// SVG box is centred on the origin (viewBox -100..100). Positive pitch (nose
// up) pushes the horizon down; a rung for pitch value P sits at local
// y = -P*K so it meets the boresight when the aircraft is at P. Bank rotates
// the assembly by -roll (right bank tilts the horizon's right side up, matching
// the out-the-window view).

import { useFlightTelemetryContext } from "@/hooks/flight-telemetry-context";

/** SVG units per pitch degree. */
const K = 2.4;
const RAD_TO_DEG = 180 / Math.PI;

/** Pitch-ladder rungs (degrees). Climb solid, dive dashed. */
const PITCH_RUNGS = [-30, -20, -10, 10, 20, 30] as const;

/** Fixed roll-scale marks (on-screen degrees from top). */
const ROLL_MARKS = [-60, -45, -30, -20, -10, 10, 20, 30, 45, 60] as const;

function PitchRung({ deg }: { deg: number }) {
  const y = -deg * K;
  const dashed = deg < 0;
  const half = 22;
  const gap = 8;
  const label = String(Math.abs(deg));
  return (
    <g>
      <line
        x1={-half}
        y1={y}
        x2={-gap}
        y2={y}
        strokeDasharray={dashed ? "4 3" : undefined}
      />
      <line
        x1={gap}
        y1={y}
        x2={half}
        y2={y}
        strokeDasharray={dashed ? "4 3" : undefined}
      />
      <text
        x={-half - 3}
        y={y}
        textAnchor="end"
        dominantBaseline="middle"
        fontSize="7"
        stroke="none"
        fill="currentColor"
      >
        {label}
      </text>
      <text
        x={half + 3}
        y={y}
        textAnchor="start"
        dominantBaseline="middle"
        fontSize="7"
        stroke="none"
        fill="currentColor"
      >
        {label}
      </text>
    </g>
  );
}

function Boresight() {
  return (
    <g className="text-amber" stroke="currentColor" strokeWidth="2.2" fill="none">
      <line x1={-28} y1={0} x2={-10} y2={0} />
      <line x1={10} y1={0} x2={28} y2={0} />
      <circle cx={0} cy={0} r={2.6} strokeWidth="1.8" />
    </g>
  );
}

export function AttitudeIndicator() {
  const { telemetry, live } = useFlightTelemetryContext();
  const att = telemetry?.attitude;
  const rollDeg = att?.roll != null ? att.roll * RAD_TO_DEG : 0;
  const pitchDeg = att?.pitch != null ? att.pitch * RAD_TO_DEG : 0;

  return (
    <svg
      viewBox="-100 -100 200 200"
      preserveAspectRatio="xMidYMid meet"
      className="h-full w-full"
      style={{ filter: "drop-shadow(0 0 1.5px rgba(0,0,0,0.9))" }}
      aria-hidden
    >
      {live ? (
        <>
          <defs>
            <clipPath id="hud-window">
              <rect x={-95} y={-72} width={190} height={144} rx={6} />
            </clipPath>
          </defs>

          {/* horizon + pitch ladder, clipped to a central window */}
          <g clipPath="url(#hud-window)">
            <g
              className="text-amber"
              stroke="currentColor"
              strokeWidth="1.4"
              fill="none"
              transform={`rotate(${-rollDeg}) translate(0 ${pitchDeg * K})`}
            >
              {/* horizon line with a centre gap for the boresight */}
              <line x1={-190} y1={0} x2={-16} y2={0} strokeWidth="1.8" />
              <line x1={16} y1={0} x2={190} y2={0} strokeWidth="1.8" />
              {PITCH_RUNGS.map((deg) => (
                <PitchRung key={deg} deg={deg} />
              ))}
            </g>
          </g>

          {/* fixed roll scale */}
          <g
            className="text-surface-foreground"
            stroke="currentColor"
            strokeWidth="1.2"
            opacity={0.85}
          >
            {ROLL_MARKS.map((mark) => {
              const major = Math.abs(mark) >= 30;
              return (
                <line
                  key={mark}
                  x1={0}
                  y1={-82}
                  x2={0}
                  y2={major ? -76 : -79}
                  transform={`rotate(${mark})`}
                />
              );
            })}
            {/* top zero notch */}
            <polygon
              points="0,-83 -3,-88 3,-88"
              fill="currentColor"
              stroke="none"
            />
          </g>

          {/* moving roll pointer — rotates with the horizon */}
          <g
            className="text-amber"
            transform={`rotate(${-rollDeg})`}
            fill="currentColor"
            stroke="none"
          >
            <polygon points="0,-74 -3.4,-68 3.4,-68" />
          </g>
        </>
      ) : (
        <g
          className="text-muted-foreground"
          stroke="currentColor"
          strokeWidth="1.2"
          fill="none"
          opacity={0.6}
        >
          <line x1={-40} y1={0} x2={-14} y2={0} strokeDasharray="4 4" />
          <line x1={14} y1={0} x2={40} y2={0} strokeDasharray="4 4" />
          <text
            x={0}
            y={-18}
            textAnchor="middle"
            fontSize="8"
            stroke="none"
            fill="currentColor"
            letterSpacing="1"
          >
            NO ATTITUDE
          </text>
        </g>
      )}

      <Boresight />
    </svg>
  );
}
