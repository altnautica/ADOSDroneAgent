// A ticking clock for freshness gates, so a surface ages its inputs
// out even when no new data arrives to re-render it. Reading the clock from
// state (not `Date.now()` in render) keeps renders pure. A null interval
// stops the tick (nothing to age).

import { useEffect, useState } from "react";

/** Which clock the tick reads: the wall clock (epoch ms, comparable with
 *  `Date.now()` stamps) or the monotonic `performance.now()` clock (for
 *  stamps taken on it; it never jumps with a suspend or a clock slew). */
export type NowClock = "wall" | "monotonic";

export function useNow(intervalMs: number | null, clock: NowClock = "wall"): number {
  const [now, setNow] = useState(() => (clock === "monotonic" ? performance.now() : Date.now()));
  useEffect(() => {
    if (intervalMs === null) return;
    const id = setInterval(() => setNow(clock === "monotonic" ? performance.now() : Date.now()), intervalMs);
    return () => clearInterval(id);
  }, [intervalMs, clock]);
  return now;
}
