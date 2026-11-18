// A ticking clock for freshness gates, so a surface ages its inputs
// out even when no new data arrives to re-render it. Reading the clock from
// state (not `Date.now()` in render) keeps renders pure. A null interval
// stops the tick (nothing to age). The tick lands on the cockpit's redraw
// beat, rounded to whole beats, so every clock on screen updates in the
// same frame.

import { useEffect, useState } from "react";

import { BEAT_MS, subscribeBeat } from "@/lib/redraw-beat";

/** Which clock the tick reads: the wall clock (epoch ms, comparable with
 *  `Date.now()` stamps) or the monotonic `performance.now()` clock (for
 *  stamps taken on it; it never jumps with a suspend or a clock slew). */
export type NowClock = "wall" | "monotonic";

function read(clock: NowClock): number {
  return clock === "monotonic" ? performance.now() : Date.now();
}

export function useNow(intervalMs: number | null, clock: NowClock = "wall"): number {
  const [now, setNow] = useState(() => read(clock));
  useEffect(() => {
    if (intervalMs === null) return;
    const every = Math.max(1, Math.round(intervalMs / BEAT_MS));
    let beats = 0;
    return subscribeBeat(() => {
      beats += 1;
      if (beats < every) return;
      beats = 0;
      setNow(read(clock));
    });
  }, [intervalMs, clock]);
  return now;
}
