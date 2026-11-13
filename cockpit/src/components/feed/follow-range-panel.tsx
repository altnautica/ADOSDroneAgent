// The range panel (13-hud-v2 §8.1, L2, read-only): bottom-centre over the
// video, above the bottom chrome, shown in following mode. Range and phase,
// the ±2σ interval bar, the target geometry, and the range cues with the
// pipeline latency and the re-acquire mode; height above ground is the bottom
// row's alone. Rows by width class: one merged line at compact, two rows at
// medium, four above.

import type { WidthClass } from "@/lib/cockpit-layout";
import { compactFollowLine, followPanelModel } from "@/lib/follow-panel";
import { cn } from "@/lib/utils";
import type { PhaseTone } from "@/shared/hud/follow-chrome";
import type { FollowState } from "@/shared/hud/nav-types";
import { specText } from "@/shared/hud/spec-text";

const PHASE_CLASS: Record<PhaseTone | "warn", string> = {
  track: "text-hud-track",
  coast: "text-hud-coast",
  warn: "text-hud-warn",
  crit: "text-hud-crit",
  muted: "text-hud-muted",
};

export function FollowRangePanel({
  follow,
  ageMs,
  widthClass,
}: {
  follow: FollowState | null;
  ageMs: number;
  widthClass: WidthClass;
}) {
  const m = followPanelModel(follow, ageMs);
  const plate = cn(
    "pointer-events-none rounded-lg border border-hud-hair bg-hud-glass-strong font-mono backdrop-blur-hud",
    m.stale && "opacity-50",
  );

  if (widthClass === "compact") {
    return (
      <div className={cn(plate, "px-[10px] py-[4px] text-[13px] text-hud-ink")} role="status" aria-label={specText("follow.range")}>
        {compactFollowLine(m)} <span className={PHASE_CLASS[m.phaseTone]}>{m.phase}</span>
      </div>
    );
  }

  const full = widthClass === "expanded" || widthClass === "tv";
  const bigPx = widthClass === "tv" ? 34 : 28;
  return (
    <div
      className={cn(plate, "flex flex-col gap-[5px] px-[13px] py-[8px] text-hud-ink")}
      style={{ width: widthClass === "tv" ? 400 : full ? 320 : undefined }}
      role="status"
      aria-label={specText("follow.range")}
    >
      <div className="flex items-baseline justify-between gap-[13px]">
        <span className="font-semibold leading-none" style={{ fontSize: bigPx }}>
          {m.range}
        </span>
        <span className={cn("text-[11px] font-semibold tracking-hud-tag", PHASE_CLASS[m.phaseTone])}>{m.phase}</span>
      </div>
      <div className="flex items-center gap-[8px]">
        <svg width={160} height={10} className="shrink-0" aria-hidden>
          <rect x={0} y={3} width={160} height={4} rx={2} fill="var(--hud-hair)" />
          {m.bar && m.bar.lo !== null && m.bar.hi !== null ? (
            <rect x={m.bar.lo * 160} y={3} width={Math.max(1, (m.bar.hi - m.bar.lo) * 160)} height={4} rx={2} fill="var(--hud-track)" />
          ) : null}
          {m.bar ? <rect x={m.bar.tick * 160 - 1} y={0} width={2} height={10} fill="var(--hud-ink)" /> : null}
        </svg>
        <span className="whitespace-nowrap text-[12px] text-hud-ink-2">{m.interval ?? "—"}</span>
      </div>
      {full ? (
        <>
          <div className="text-[12px] text-hud-ink-2">{m.geometry}</div>
          <div className="flex flex-wrap items-center gap-[6px] text-[11px]">
            {m.cues.map((c) => (
              <span
                key={c.label}
                className={cn(
                  "rounded border border-hud-hair px-[5px]",
                  c.lit ? "bg-hud-glass text-hud-ink" : "text-hud-muted line-through",
                )}
              >
                {c.label}
              </span>
            ))}
            <span className={m.latencyWarn ? "text-hud-warn" : "text-hud-ink-2"}>{m.latency}</span>
            {m.mode ? <span className="text-hud-ink-2">{m.mode}</span> : null}
          </div>
        </>
      ) : null}
    </div>
  );
}
