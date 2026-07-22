// A compact 4-bar RSSI strength meter for the status strip. Bars fill by signal
// strength; a null reading (no packet decoded yet) shows every bar muted rather
// than a fabricated level. Purely presentational.

import type { Tone } from "@/components/ui/data";
import { cn } from "@/lib/utils";

const BAR_HEIGHTS = ["0.32rem", "0.46rem", "0.6rem", "0.74rem"];

/** Map an RSSI dBm reading to a filled-bar count 0..4. Rough WFB scale: ≥ -55
 *  excellent (4), -65 good (3), -75 fair (2), -85 weak (1), below that / null
 *  → 0 (all muted). */
function rssiBars(rssi: number | null | undefined): number {
  if (rssi == null || !Number.isFinite(rssi)) return 0;
  if (rssi >= -55) return 4;
  if (rssi >= -65) return 3;
  if (rssi >= -75) return 2;
  if (rssi >= -85) return 1;
  return 0;
}

function fillClass(tone: Tone): string {
  switch (tone) {
    case "ok":
      return "bg-ok";
    case "warn":
      return "bg-warn";
    case "err":
      return "bg-err";
    default:
      return "bg-muted-foreground";
  }
}

export function SignalBars({
  rssi,
  tone = "ok",
}: {
  rssi: number | null | undefined;
  tone?: Tone;
}) {
  const filled = rssiBars(rssi);
  return (
    <span className="inline-flex items-end gap-[0.09rem]" aria-hidden>
      {BAR_HEIGHTS.map((h, i) => (
        <span
          key={i}
          className={cn("w-[0.16rem] rounded-[1px]", i < filled ? fillClass(tone) : "bg-input")}
          style={{ height: h }}
        />
      ))}
    </span>
  );
}
