// A live wall clock (HH:MM, 24-hour, with the zone) for the status strip's box
// zone, so a display-only operator with no keyboard has a time reference on
// screen. It re-reads the local clock every 10 s (a minute-granularity display
// never needs finer). Uses the panel's own local time and names its zone, so it
// cannot be mistaken for UTC; it makes no claim about a synced source.

import { useNow } from "@/hooks/use-now";
import { clockText } from "@/lib/clock-text";

export function WallClock() {
  const now = useNow(10_000);
  return <span className="font-mono tabular-nums">{clockText(new Date(now))}</span>;
}
