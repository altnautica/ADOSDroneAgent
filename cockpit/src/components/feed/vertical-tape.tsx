// A rolling vertical tape — speed on the left, altitude on the right. The scale
// slides so the current value stays centred against a fixed readout box; higher
// values are up. Positioning is pure percentage against the tape height (a tick
// `windowSpan/2` above the value sits at the top), so it scales fluidly with the
// panel and needs no measurement. When the value is unknown (no live telemetry)
// it shows the frame + a dash readout and no numbered scale — never a fabricated
// number line.

import { cn } from "@/lib/utils";
import { DASH } from "@/lib/format";

export interface VerticalTapeProps {
  side: "left" | "right";
  label: string;
  unit: string;
  /** Sanitised current value (null when unknown). */
  value: number | null;
  /** Total value span mapped across the tape height. */
  windowSpan: number;
  /** Numbered-tick spacing. */
  step: number;
}

function visibleTicks(value: number, windowSpan: number, step: number): number[] {
  const half = windowSpan / 2;
  const lo = Math.ceil((value - half) / step) * step;
  const hi = value + half;
  const ticks: number[] = [];
  for (let t = lo; t <= hi + 1e-6; t += step) {
    ticks.push(Math.round(t));
  }
  return ticks;
}

export function VerticalTape({
  side,
  label,
  unit,
  value,
  windowSpan,
  step,
}: VerticalTapeProps) {
  const isLeft = side === "left";
  const hasValue = value != null && Number.isFinite(value);
  const ticks = hasValue ? visibleTicks(value, windowSpan, step) : [];

  return (
    <div className="relative h-full w-full text-surface-foreground">
      {/* label */}
      <div
        className={cn(
          "absolute top-0 text-[0.62rem] uppercase tracking-wide text-muted-foreground",
          isLeft ? "left-0" : "right-0",
        )}
      >
        {label}
      </div>

      {/* the baseline the ticks hang off, on the inner (centre-facing) edge */}
      <div
        className={cn(
          "absolute bottom-[1.1rem] top-[1.1rem] w-px bg-surface-foreground/40",
          isLeft ? "right-0" : "left-0",
        )}
      />

      {/* the sliding numbered scale */}
      <div className="pointer-events-none absolute bottom-[1.1rem] left-0 right-0 top-[1.1rem] overflow-hidden">
        {ticks.map((tick) => {
          const topPct = 50 - ((tick - (value as number)) / windowSpan) * 100;
          if (topPct < -2 || topPct > 102) return null;
          return (
            <div
              key={tick}
              className={cn(
                "absolute flex -translate-y-1/2 items-center gap-[0.2rem]",
                isLeft ? "right-0 flex-row" : "left-0 flex-row-reverse",
              )}
              style={{ top: `${topPct}%` }}
            >
              <span className="font-mono text-[0.6rem] text-surface-foreground/80">
                {tick}
              </span>
              <span className="h-px w-[0.4rem] bg-surface-foreground/50" />
            </div>
          );
        })}
      </div>

      {/* fixed centre readout */}
      <div
        className={cn(
          "absolute top-1/2 flex -translate-y-1/2 items-baseline gap-[0.15rem] rounded-sm bg-background/70 px-[0.3rem] py-[0.1rem] backdrop-blur-sm",
          isLeft ? "left-0" : "right-0",
        )}
      >
        <span className="font-mono text-[0.95rem] font-semibold text-amber">
          {hasValue ? Math.round(value as number) : DASH}
        </span>
        <span className="text-[0.55rem] text-muted-foreground">{unit}</span>
      </div>
    </div>
  );
}
