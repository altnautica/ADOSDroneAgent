// Multi-stream tabs, top-left over the feed. Shown only when the node reports
// more than one camera; each tab selects the active stream, and the Feed
// re-points the video layer to it. A ground station has no onboard camera and
// returns an empty roster, so these never appear there. Positioned to clear the
// top status strip and the left menu rail.

import { useFeedStore } from "@/stores/feed-store";
import { useNavStore } from "@/stores/nav-store";
import type { RosterCamera } from "@/lib/types";
import { cn } from "@/lib/utils";

function cameraLabel(cam: RosterCamera): string {
  return cam.label ?? cam.name ?? cam.role ?? cam.id;
}

export function StreamTabs({ cameras }: { cameras: RosterCamera[] }) {
  const activeCameraId = useFeedStore((s) => s.activeCameraId);
  const setActiveCamera = useFeedStore((s) => s.setActiveCamera);
  const menuCollapsed = useNavStore((s) => s.menuCollapsed);

  // Default to the first camera when nothing is selected yet.
  const activeId = activeCameraId ?? cameras[0]?.id ?? null;

  const leftInset = menuCollapsed
    ? "left-[0.5rem]"
    : "left-[0.5rem] landscape:left-[6.9rem]";

  return (
    <div
      className={cn(
        "pointer-events-auto absolute top-[2.7rem] flex gap-[0.3rem]",
        leftInset,
      )}
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
              "rounded-md px-[0.55rem] py-[0.3rem] text-[0.7rem] font-medium backdrop-blur-sm transition-colors",
              active
                ? "bg-amber text-amber-foreground"
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
