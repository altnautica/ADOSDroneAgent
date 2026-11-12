// The controller drawing in Settings › Controls: the spec art, live input
// (pressed controls fill, stick dots and deadzone rings, L2/R2 bars), the
// leader-line label from each hotspot to its bound action, and the hotspots
// the operator picks a control with.

import { useRef } from "react";

import { useElementSize } from "@/hooks/use-element-size";
import { isKiosk } from "@/lib/kiosk";
import { DRAWINGS, PAD_COLORS, padKindOf, type Hotspot } from "@/lib/pad/art";
import { CALLOUT_COLUMN, fitLabel, hitRadii, layoutCallouts, pathBox } from "@/lib/pad/callout-layout";
import { BUTTON_INDEX, CONTROL_IDS, CONTROL_LABELS, type ControlId, type PadBinding } from "@/lib/pad/ds4-map";
import { hotspotLabel } from "@/lib/pad/labels";
import { specText } from "@/shared/hud/spec-text";
import { useControlsStore } from "@/stores/controls-store";
import { useInputStore, type PadSample } from "@/stores/input-store";

/** Smallest tap target for a drawn control, in CSS pixels (48 on the kiosk). */
const HIT_PX = 36;
const HIT_KIOSK_PX = 48;

function pressedControl(pad: PadSample | null, control: ControlId): boolean {
  if (!pad) return false;
  if (control === "l2") return pad.l2 > 0.05;
  if (control === "r2") return pad.r2 > 0.05;
  const i = BUTTON_INDEX[control];
  return i !== undefined && pad.pressed[i] === true;
}

function HotspotShape({ h, ...rest }: { h: Hotspot } & React.SVGProps<SVGElement>) {
  const props = rest as React.SVGProps<SVGCircleElement & SVGRectElement & SVGPathElement>;
  if (h.shape === "circle") return <circle cx={h.geom.cx} cy={h.geom.cy} r={h.geom.r} {...props} />;
  if (h.shape === "rect") return <rect x={h.geom.x} y={h.geom.y} width={h.geom.w} height={h.geom.h} rx={h.geom.rx} {...props} />;
  return <path d={h.geom.d} {...props} />;
}

function hotspotCentre(h: Hotspot): [number, number] {
  if (h.shape === "circle") return [h.geom.cx, h.geom.cy];
  if (h.shape === "rect") return [h.geom.x + h.geom.w / 2, h.geom.y + h.geom.h / 2];
  return pathBox(h.geom.d).centre;
}

/** The drawn control's own radius: a circle's, or half a rect's short side. */
function drawnRadius(h: Hotspot): number {
  if (h.shape === "circle") return h.geom.r;
  if (h.shape === "rect") return Math.min(h.geom.w, h.geom.h) / 2;
  return pathBox(h.geom.d).radius;
}

export function PadDrawingView({
  rows,
  selected,
  onSelect,
  conflicted,
  deadzone,
  registry,
}: {
  rows: readonly PadBinding[];
  selected: ControlId | null;
  onSelect: (c: ControlId) => void;
  conflicted: ReadonlySet<ControlId>;
  deadzone: number;
  registry: ReadonlyMap<string, string>;
}) {
  const pad = useInputStore((s) => s.pad);
  const kind = pad ? padKindOf(pad.id) : useControlsStore.getState().pad;
  const d = DRAWINGS[kind];
  const [vx, vy, vw, vh] = d.art.viewBox;
  const boxRef = useRef<HTMLDivElement>(null);
  const { w: boxW } = useElementSize(boxRef);
  const viewW = vw + 2 * CALLOUT_COLUMN;
  // viewBox units per CSS pixel at the drawn width (the drawing caps at 960 px).
  const unitsPerPx = boxW > 0 ? viewW / Math.min(boxW, 960) : viewW / 960;
  const wantedHit = (isKiosk() ? HIT_KIOSK_PX : HIT_PX) * unitsPerPx;
  const ids = CONTROL_IDS.filter((c) => d.hotspots[c]);
  const centres: Record<string, [number, number]> = {};
  const drawnR: Record<string, number> = {};
  for (const c of ids) {
    const h = d.hotspots[c]!;
    centres[c] = hotspotCentre(h);
    drawnR[c] = drawnRadius(h);
  }
  const hits = hitRadii(centres, drawnR, wantedHit);
  // Larger controls first, so a small one drawn over a large one (L3 inside
  // its stick) keeps its own tap.
  const order = [...ids].sort((a, b) => drawnR[b] - drawnR[a]);
  const callouts = layoutCallouts(
    ids.flatMap((c) => {
      const label = hotspotLabel(rows, c, registry);
      const h = d.hotspots[c]!;
      return label ? [{ control: c, label, labelSide: h.label_side, anchor: h.label_anchor }] : [];
    }),
    { x: vx, y: vy, w: vw, h: vh },
  );
  return (
    <div ref={boxRef} className="w-full max-w-[960px]">
    <svg viewBox={`${vx - CALLOUT_COLUMN} ${vy} ${viewW} ${vh}`} className="w-full" role="img" aria-label={specText("controls.drawing")} data-testid="pad-drawing">
      {d.art.paths.map((p) => {
        const on = p.control ? pressedControl(pad, p.control) : false;
        return (
          <path
            key={p.id}
            d={p.d}
            fill={on ? PAD_COLORS.pressed : "none"}
            stroke={PAD_COLORS.art_stroke}
            strokeWidth={d.art.strokeWidth}
            strokeLinecap="round"
            strokeLinejoin="round"
            opacity={p.role === "detail" ? 0.6 : 1}
          />
        );
      })}
      {(["stick_l", "stick_r"] as const).map((c) => {
        const h = d.hotspots[c];
        if (!h || h.shape !== "circle") return null;
        const s = pad ? (c === "stick_l" ? pad.sticks.l : pad.sticks.r) : ([0, 0] as const);
        return (
          <g key={c} pointerEvents="none">
            <circle cx={h.geom.cx} cy={h.geom.cy} r={h.geom.r * deadzone} fill="none" stroke={PAD_COLORS.deadzone_ring} strokeDasharray="4 4" />
            <circle cx={h.geom.cx + s[0] * h.geom.r} cy={h.geom.cy + s[1] * h.geom.r} r={8} fill={PAD_COLORS.pressed} data-testid={`${c}-dot`} />
          </g>
        );
      })}
      {(["l2", "r2"] as const).map((c) => {
        const h = d.hotspots[c];
        if (!h) return null;
        const [cx, cy] = hotspotCentre(h);
        const v = pad ? (c === "l2" ? pad.l2 : pad.r2) : 0;
        return (
          <g key={`${c}-bar`} pointerEvents="none">
            <rect x={cx - 30} y={cy - 46} width={60} height={8} fill="none" stroke={PAD_COLORS.art_stroke} />
            <rect x={cx - 30} y={cy - 46} width={60 * v} height={8} fill={PAD_COLORS.pressed} />
          </g>
        );
      })}
      {callouts.map((co) => {
        const h = d.hotspots[co.control as ControlId]!;
        const [ax, ay] = h.label_anchor;
        const [hx, hy] = centres[co.control];
        const edge = co.side === "left" ? vx - 6 : vx + vw + 6;
        const bad = conflicted.has(co.control as ControlId);
        return (
          <g key={`callout-${co.control}`} pointerEvents="none" data-callout={co.control}>
            <polyline points={`${hx},${hy} ${ax},${ay} ${edge},${co.y}`} fill="none" stroke={bad ? PAD_COLORS.conflict : "rgba(237,237,237,0.45)"} strokeWidth={1.5} />
            <text x={co.side === "left" ? vx - 10 : vx + vw + 10} y={co.y + 6} textAnchor={co.side === "left" ? "end" : "start"} fontSize={18} fill={bad ? PAD_COLORS.conflict : "#EDEDED"}>
              <title>{co.label}</title>
              {fitLabel(co.label)}
            </text>
          </g>
        );
      })}
      {order.map((c) => {
        const h = d.hotspots[c]!;
        const bad = conflicted.has(c);
        const [cx, cy] = centres[c];
        return (
          <g
            key={c}
            role="button"
            aria-label={CONTROL_LABELS[c]}
            tabIndex={0}
            onClick={() => onSelect(c)}
            onKeyDown={(e: React.KeyboardEvent) => {
              if (e.key === "Enter") onSelect(c);
            }}
            style={{ cursor: "pointer" }}
          >
            <circle cx={cx} cy={cy} r={hits[c]} fill="transparent" data-hit={c} />
            <HotspotShape
              h={h}
              data-control={c}
              data-selected={selected === c ? "true" : undefined}
              data-conflict={bad ? "true" : undefined}
              fill={pressedControl(pad, c) ? `${PAD_COLORS.pressed}66` : "transparent"}
              stroke={bad ? PAD_COLORS.conflict : selected === c ? PAD_COLORS.pressed : "transparent"}
              strokeWidth={bad || selected === c ? 4 : 0}
            />
          </g>
        );
      })}
    </svg>
    </div>
  );
}
