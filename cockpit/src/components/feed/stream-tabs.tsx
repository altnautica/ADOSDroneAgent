// Multi-stream tabs, top-left over the feed. Shown only when the node reports
// more than one camera; each tab selects the active stream, and the Feed
// re-points the video layer to it. Shown under the safety band.

import { useFeedStore } from "@/stores/feed-store";
import type { RosterCamera } from "@/lib/types";
import { cn } from "@/lib/utils";

function cameraLabel(cam: RosterCamera): string {
  return cam.label ?? cam.name ?? cam.role ?? cam.id;
}

export function StreamTabs({ cameras }: { cameras: RosterCamera[] }) {
  const activeCameraId = useFeedStore((s) => s.activeCameraId);
  const setActiveCamera = useFeedStore((s) => s.setActiveCamera);

  // Default to the first camera when nothing is selected yet.
  const activeId = activeCameraId ?? cameras[0]?.id ?? null;

  return (
    <div
className="pointer-events-auto flex gap-[0.3rem]"
    >
      {cameras.map((cam) => {
        const active = cam.id === activeId;
        return (
          <button
            key={cam.id}
            type="button"
            onClick={() => setActiveCamera(cam.id)}
            aria-pressed={active}
            className={cn(
              "min-h-[40px] whitespace-nowrap rounded-md px-[0.55rem] py-[0.3rem] text-[0.75rem] font-medium backdrop-blur-sm transition-colors",
              active
                ? "bg-primary text-primary-foreground"
                : "bg-background/55 text-surface-foreground hover:bg-muted",
            )}
          >
            {cameraLabel(cam)}
          </button>
        );
      })}
    </div>
  );
}
