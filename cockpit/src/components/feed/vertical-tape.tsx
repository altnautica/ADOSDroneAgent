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

/** The largest magnitude a tape value is allowed to drive the scale to.
 *
 *  Far beyond any real altitude or airspeed, so it never clips a genuine
 *  reading; its job is only to keep a garbage float out of the tick loop. */
const MAX_TAPE_VALUE = 1e9;

/** Hard ceiling on generated ticks, as a second independent bound.
 *
 *  `windowSpan / step` is a small number for every call site today, so this is
 *  never reached in practice — it exists so a future caller passing a tiny step
 *  cannot hang the panel either. */
const MAX_TICKS = 512;

/** Whether a value can safely drive the tape scale.
 *
 *  `Number.isFinite` alone is NOT enough, which is the whole reason this exists:
 *  a finite-but-enormous float (a corrupt MAVLink field is a plain f32, so up to
 *  ~3.4e38) makes the tick loop below non-terminating — past ~1e17 the float ULP
 *  exceeds `step`, so `t += step` stops advancing the accumulator and the loop
 *  spins forever. It throws nothing, so the error boundary cannot catch it and
 *  the kiosk simply freezes with no diagnostic. Bounding the input is the fix;
 *  the tick cap below is the belt to its braces. */
export function isPlottableTapeValue(value: number | null | undefined): value is number {
  return value != null && Number.isFinite(value) && Math.abs(value) <= MAX_TAPE_VALUE;
}

export function visibleTicks(value: number, windowSpan: number, step: number): number[] {
  // A non-positive step would never advance the accumulator at all. No call
  // site passes one today; refusing here means none ever can.
  if (!Number.isFinite(step) || step <= 0) return [];
  const half = windowSpan / 2;
  const lo = Math.ceil((value - half) / step) * step;
  const hi = value + half;
  const ticks: number[] = [];
  // Indexed rather than accumulated: `lo + i * step` is exact at every
  // iteration and cannot stall the way a repeated `t += step` can.
  const count = Math.min(Math.floor((hi - lo) / step) + 1, MAX_TICKS);
  for (let i = 0; i < count; i += 1) {
    ticks.push(Math.round(lo + i * step));
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
  const hasValue = isPlottableTapeValue(value);
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
