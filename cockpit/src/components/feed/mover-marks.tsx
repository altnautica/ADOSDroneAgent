// The moving obstacles the pilot tracks, on the local-frame minimap: per
// mover its one-sigma ring (faint fill), an arrow to where it will be in 2 s,
// and a mark in its kind's colour and shape (person a disc, vehicle a square,
// anything else a triangle), faded once it went unmeasured for over a second.
// Positions come from the shared local scene.

import type { LocalSceneMover } from "@/shared/hud/local-scene";
import { MOVER_KIND_COLOR } from "@/shared/hud/movers";

const MARK_PX = 4.5;
const HEAD_PX = 4;
const FADED_OPACITY = 0.4;

function Glyph({ m }: { m: LocalSceneMover }) {
  if (m.kind === "person") return <circle cx={m.sx} cy={m.sy} r={MARK_PX} />;
  if (m.kind === "vehicle") {
    return <rect x={m.sx - MARK_PX} y={m.sy - MARK_PX} width={MARK_PX * 2} height={MARK_PX * 2} rx={1} />;
  }
  return (
    <polygon
      points={`${m.sx},${m.sy - MARK_PX * 1.2} ${m.sx + MARK_PX},${m.sy + MARK_PX * 0.8} ${m.sx - MARK_PX},${m.sy + MARK_PX * 0.8}`}
    />
  );
}

/** The arrow from the mover to its lead point, with a head pointing along the
 *  velocity; null while it is still. */
function Arrow({ m }: { m: LocalSceneMover }) {
  const { sx, sy, leadX: lx, leadY: ly } = m;
  if (lx === null || ly === null) return null;
  const a = Math.atan2(ly - sy, lx - sx);
  const p = (da: number) => `${lx - HEAD_PX * Math.cos(a + da)},${ly - HEAD_PX * Math.sin(a + da)}`;
  return (
    <g strokeWidth={1.5} fill="none" strokeLinecap="round" strokeLinejoin="round">
      <line x1={sx} y1={sy} x2={lx} y2={ly} />
      <polyline points={`${p(0.5)} ${lx},${ly} ${p(-0.5)}`} />
    </g>
  );
}

export function MoverMarks({ movers }: { movers: readonly LocalSceneMover[] }) {
  if (movers.length === 0) return null;
  return (
    <g data-testid="local-minimap-movers">
      {movers.map((m) => {
        const colour = MOVER_KIND_COLOR[m.kind];
        return (
          <g key={m.id} data-kind={m.kind} data-faded={m.faded || undefined} opacity={m.faded ? FADED_OPACITY : 1} stroke={colour}>
            {m.ringPx > MARK_PX ? (
              <circle cx={m.sx} cy={m.sy} r={m.ringPx} fill={colour} fillOpacity={0.12} strokeWidth={1} strokeDasharray="3 2" />
            ) : null}
            <Arrow m={m} />
            <g fill={colour} stroke="rgba(0,0,0,0.65)" strokeWidth={1}>
              <Glyph m={m} />
            </g>
          </g>
        );
      })}
    </g>
  );
}
