// The Map panel: the world-engine mapping state from the nav stream's `map`
// frames (`map_status`), opened from the minimap header. Compute placement,
// mapping memory against its budget, voxel size (it grows when the sweep
// cloud hits its budget), dropped keyframes, sweep points, mapped area, and
// upload or live-stream progress. Loaded on first open, off the Feed's chunk.

import { Sheet } from "@/components/ui/sheet";
import { useNow } from "@/hooks/use-now";
import { MAP_STATUS_FRESH_MS, mapStatusRows, type MapRowTone } from "@/lib/map-status-view";
import { cn } from "@/lib/utils";
import { ageOf, useNavStreamStore } from "@/stores/nav-stream-store";

const TONE: Record<MapRowTone, string> = {
  normal: "text-foreground",
  good: "text-hud-good",
  warn: "text-hud-warn",
  crit: "text-hud-crit",
  muted: "text-muted-foreground",
};

const BAR: Record<MapRowTone, string> = {
  normal: "bg-primary",
  good: "bg-hud-good",
  warn: "bg-hud-warn",
  crit: "bg-hud-crit",
  muted: "bg-muted",
};

export default function MapPanel({ onClose }: { onClose: () => void }) {
  const map = useNavStreamStore((s) => s.map);
  const now = useNow(1000);
  const stale = ageOf(map, now) > MAP_STATUS_FRESH_MS;
  const rows = map ? mapStatusRows(map.data) : null;

  return (
    <Sheet title="Map" onDismiss={onClose}>
      <div className="flex flex-col gap-[0.6rem] text-[0.85rem]">
        {rows === null ? (
          <p className="text-muted-foreground">No mapping session reported. The map appears when the lidar mapper runs.</p>
        ) : (
          <>
            {stale ? (
              <p className="font-mono text-[0.8rem] text-hud-warn">STALE · last report {Math.round(ageOf(map, now) / 1000)} s ago</p>
            ) : null}
            <dl className={cn("grid grid-cols-[auto_1fr] gap-x-[1rem] gap-y-[0.45rem]", stale && "opacity-70")}>
              {rows.map((r) => (
                <div key={r.id} className="contents">
                  <dt className="text-muted-foreground">{r.label}</dt>
                  <dd className={cn("flex flex-col gap-[0.2rem] font-mono", TONE[r.tone])}>
                    {r.value}
                    {r.fraction !== undefined ? (
                      <span className="h-[4px] w-full overflow-hidden rounded-sm bg-hud-hair" aria-hidden>
                        <span className={cn("block h-full", BAR[r.tone])} style={{ width: `${r.fraction * 100}%` }} />
                      </span>
                    ) : null}
                  </dd>
                </div>
              ))}
            </dl>
          </>
        )}
      </div>
    </Sheet>
  );
}
