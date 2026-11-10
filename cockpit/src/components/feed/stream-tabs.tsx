// The Menu's Cameras entry: one button per video leg the node offers, by its
// human label, the selected one pressed. Choosing a leg re-points the Feed's
// video layer to it. The switch lives in the Menu, never over the video.

import { cameraLabel } from "@/lib/feed-legs";
import type { RosterCamera } from "@/lib/types";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { useFeedStore } from "@/stores/feed-store";

export function CameraSwitcher({ cameras }: { cameras: RosterCamera[] }) {
  const activeCameraId = useFeedStore((s) => s.activeCameraId);
  const setActiveCamera = useFeedStore((s) => s.setActiveCamera);

  // Default to the first camera when nothing is selected yet.
  const activeId = activeCameraId ?? cameras[0]?.id ?? null;

  return (
    <div role="group" aria-label={specText("cockpit.menu.cameras")} className="flex flex-wrap gap-[6px]" data-testid="menu-cameras">
      {cameras.map((cam) => {
        const active = cam.id === activeId;
        return (
          <button
            key={cam.id}
            type="button"
            onClick={() => setActiveCamera(cam.id)}
            aria-pressed={active}
            className={cn(
              "chrome-bar-text min-h-[48px] min-w-[48px] whitespace-nowrap rounded-[var(--fs-radius-sm)] border px-[12px] font-semibold",
              active ? "border-[var(--fs-ok)] bg-white/15" : "border-[var(--fs-border)] hover:bg-white/10",
            )}
          >
            {cameraLabel(cam)}
          </button>
        );
      })}
    </div>
  );
}
