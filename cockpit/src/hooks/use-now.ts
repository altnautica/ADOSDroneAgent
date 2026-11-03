// A ticking epoch-ms clock for freshness gates, so a surface ages its inputs
// out even when no new data arrives to re-render it. Reading the clock from
// state (not `Date.now()` in render) keeps renders pure. A null interval
// stops the tick (nothing to age).

import { useEffect, useState } from "react";

export function useNow(intervalMs: number | null): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (intervalMs === null) return;
    const id = setInterval(() => setNow(Date.now()), intervalMs);
    return () => clearInterval(id);
  }, [intervalMs]);
  return now;
}
