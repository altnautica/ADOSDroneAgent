// A live wall clock (HH:MM, 24-hour) for the status strip's box zone, so a
// display-only operator with no keyboard has a time reference on screen. It
// re-reads the local clock every 10 s (a minute-granularity display never needs
// finer). Uses the panel's own local time; it makes no claim about a synced
// source.

import { useEffect, useState } from "react";

function hhmm(d: Date): string {
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

export function WallClock() {
  const [now, setNow] = useState(() => hhmm(new Date()));
  useEffect(() => {
    const id = setInterval(() => setNow(hhmm(new Date())), 10_000);
    return () => clearInterval(id);
  }, []);
  return <span className="font-mono tabular-nums">{now}</span>;
}
