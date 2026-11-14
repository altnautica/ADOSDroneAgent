// A radar's header word: the scanning lidar's state from the pilot kind's
// `sensors.lidar` (`Lidar 9.9 Hz`, `Lidar lost`, `No lidar`) in its state
// colour (`sensor-health.json`). Nothing shows without a fresh pilot frame
// carrying `sensors`. The rate is the lidar's scan rate, never the rate the
// obstacle distances are sent at.

import { useNow } from "@/hooks/use-now";
import { usePilot } from "@/hooks/use-pilot";
import { radarLidarWord } from "@/lib/sensor-words";
import { cn } from "@/lib/utils";

export function RadarLidarHeader({ className }: { className?: string }) {
  const pilot = usePilot();
  const now = useNow(pilot ? 500 : null);
  const word = radarLidarWord(pilot, now);
  if (!word) return null;
  return (
    <span
      className={cn("pointer-events-none absolute z-10 whitespace-nowrap font-mono text-[11px] leading-none", className)}
      style={{ color: word.hex }}
      data-testid="radar-lidar"
      data-state={word.state}
    >
      {word.text}
    </span>
  );
}
