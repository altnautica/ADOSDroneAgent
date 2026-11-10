// The target layer's glyphs: one item's SVG marks and its HTML word tags, in
// the HUD v2 symbology; the spoken label is in `target-spoken.ts`.

import { memo } from "react";

import { cn } from "@/lib/utils";
import type { GlyphLayout } from "@/shared/hud/target-layout";
import type { TagTone, TargetKind } from "@/shared/hud/target-state";

const STROKE: Record<TargetKind, string> = {
  CANDIDATE: "var(--hud-ink)",
  TRACKED: "var(--hud-primary)",
  LOCKED: "var(--hud-track)",
  ACQUIRING: "var(--hud-track)",
  FOLLOWING: "var(--hud-track)",
  HOLDING: "var(--hud-track)",
  COASTING: "var(--hud-coast)",
  SEARCHING: "var(--hud-coast)",
  SEARCHING_OFFFRAME: "var(--hud-coast)",
  LOST: "var(--hud-crit)",
  GT: "var(--hud-gt)",
};

const TAG_TONE: Record<TagTone, string> = {
  ink: "bg-hud-glass text-hud-ink",
  primary: "bg-hud-glass text-hud-primary",
  track: "bg-hud-glass text-hud-track",
  trackFilled: "bg-hud-track text-black",
  warnFilled: "bg-hud-warn/85 text-black",
  coast: "bg-hud-glass text-hud-coast",
  crit: "bg-hud-glass text-hud-crit",
  warn: "bg-hud-glass text-hud-warn",
  muted: "bg-hud-glass text-hud-muted",
  gt: "bg-hud-glass text-hud-gt",
};

const OUTLINE = "var(--hud-target-outline-color)";

/** A path painted twice: the dark outline, then the state colour (§2.2). */
function Outlined({ d, color, width, dash }: { d: string; color: string; width: number; dash?: string }) {
  return (
    <>
      <path d={d} stroke={OUTLINE} strokeWidth={width + 2} strokeDasharray={dash} fill="none" strokeLinecap="butt" strokeLinejoin="miter" vectorEffect="non-scaling-stroke" />
      <path d={d} stroke={color} strokeWidth={width} strokeDasharray={dash} fill="none" strokeLinecap="butt" strokeLinejoin="miter" vectorEffect="non-scaling-stroke" />
    </>
  );
}

/** One drawn item's marks (SVG). */
export const GlyphMarks = memo(function GlyphMarks({ g, minimal }: { g: GlyphLayout; minimal: boolean }) {
  const color = STROKE[g.kind];
  const fade = g.kind === "LOST" ? 1 - (g.item.lostProgress ?? 0) : 1;
  const box = g.box;
  return (
    <g opacity={fade} data-ease-key={g.key}>
      {g.kind === "CANDIDATE" && box ? (
        <g opacity={0.7}>
          <rect x={box.x} y={box.y} width={box.w} height={box.h} stroke={OUTLINE} strokeWidth={3} fill="none" />
          <rect x={box.x} y={box.y} width={box.w} height={box.h} stroke={color} strokeWidth={1} fill="none" />
        </g>
      ) : null}
      {g.kind === "GT" && box ? (
        <rect x={box.x} y={box.y} width={box.w} height={box.h} stroke={color} strokeWidth={1.5} strokeDasharray="1.5 3" strokeLinecap="round" fill="none" />
      ) : null}
      {g.brackets ? (
        <g
          opacity={g.item.modifiers.uncertain ? 0.55 : 1}
          className={cn(
            g.item.modifiers.reacquired && "origin-center animate-in zoom-in-[1.15] duration-[320ms] ease-out motion-reduce:animate-none",
          )}
          style={{ transformBox: "fill-box" }}
        >
          <Outlined d={g.brackets} color={color} width={2.5} dash={g.kind === "COASTING" ? "4 3" : undefined} />
        </g>
      ) : null}
      {g.reticle ? <Outlined d={g.reticle} color="var(--hud-ink)" width={2} /> : null}
      {g.feet ? <Outlined d={g.feet} color="var(--hud-track)" width={2.5} /> : null}
      {g.ghost.length > 1 && !minimal ? (
        <polyline
          points={g.ghost.map((p) => `${p.x},${p.y}`).join(" ")}
          stroke="var(--hud-coast)"
          strokeWidth={1.5}
          strokeDasharray="1.5 3"
          strokeLinecap="round"
          opacity={0.6}
          fill="none"
        />
      ) : null}
      {g.ring ? (
        <g>
          <circle cx={g.ring.cx} cy={g.ring.cy} r={g.ring.r} stroke={OUTLINE} strokeWidth={3.5} fill="none" />
          <circle
            cx={g.ring.cx}
            cy={g.ring.cy}
            r={g.ring.r}
            stroke={color}
            strokeWidth={1.5}
            fill="none"
            className="motion-reduce:[stroke-dasharray:4_3]"
          />
          <g className="origin-center animate-spin [animation-duration:1.2s] motion-reduce:hidden" style={{ transformBox: "fill-box" }}>
            <circle cx={g.ring.cx} cy={g.ring.cy} r={g.ring.r} fill="none" stroke="none" />
            <path
              d={`M ${g.ring.cx},${g.ring.cy - g.ring.r} A ${g.ring.r},${g.ring.r} 0 0 1 ${g.ring.cx + g.ring.r},${g.ring.cy}`}
              stroke={color}
              strokeWidth={3}
              fill="none"
            />
          </g>
        </g>
      ) : null}
      {g.arrow ? <path d={g.arrow.path} fill={color} stroke={OUTLINE} strokeWidth={1} /> : null}
    </g>
  );
});

/** One drawn item's word tags (HTML, so the type is crisp and blurs its plate). */
export function GlyphTags({ g }: { g: GlyphLayout }) {
  const fade = g.kind === "LOST" ? 1 - (g.item.lostProgress ?? 0) : 1;
  const tags = [...(g.topTag ? [g.topTag] : []), ...g.bottomTags];
  return (
    <div data-ease-key={g.key} className="absolute inset-0">
      {tags.map((t, i) => (
        <span
          key={i}
          className={cn(
            "absolute whitespace-nowrap rounded-[4px] text-center font-mono font-semibold uppercase tracking-hud-tag backdrop-blur-hud",
            TAG_TONE[t.tone],
          )}
          style={{
            left: t.x,
            top: t.y,
            width: t.w,
            height: t.h,
            lineHeight: `${t.h}px`,
            fontSize: g.fontPx,
            opacity: fade,
          }}
        >
          {t.text}
        </span>
      ))}
    </div>
  );
}
