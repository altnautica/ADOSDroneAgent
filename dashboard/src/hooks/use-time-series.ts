import { useEffect, useRef, useState } from "react";

// Generic ring-buffer collector for sparklines. The sample function
// runs on every tick of `dependencies` and pushes whatever it returns
// (skipped when null) onto a fixed-size ring keyed by epoch seconds.
//
// We keep the latest N samples in component state via an immutable
// snapshot so the chart re-renders. The underlying ring is held in a
// ref to avoid re-rendering N times per tick.

export interface TimePoint {
  time: number; // unix seconds
  value: number;
}

interface Options {
  /** maximum number of points kept (drives the visible window) */
  maxPoints?: number;
}

export function useTimeSeries<TDep>(
  dep: TDep,
  sample: (dep: TDep) => number | null | undefined,
  { maxPoints = 60 }: Options = {},
): TimePoint[] {
  const ringRef = useRef<TimePoint[]>([]);
  const [view, setView] = useState<TimePoint[]>([]);

  useEffect(() => {
    const value = sample(dep);
    if (value == null || Number.isNaN(value)) return;

    const now = Math.floor(Date.now() / 1000);
    const ring = ringRef.current;
    const last = ring[ring.length - 1];
    // Coalesce to one sample per second so lightweight-charts doesn't
    // see duplicate timestamps on dependency changes that fire faster
    // than our 1-second axis resolution.
    if (last && last.time === now) {
      ring[ring.length - 1] = { time: now, value };
    } else {
      ring.push({ time: now, value });
      if (ring.length > maxPoints) ring.shift();
    }
    setView([...ring]);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dep]);

  return view;
}
