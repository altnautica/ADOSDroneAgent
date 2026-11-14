// The Menu's Cameras entry: one button per video leg the node offers, by its
// human label, the selected one pressed. Choosing a leg re-points the Feed's
// video layer to it. The switch lives in the Menu, never over the video. A
// stale or lost camera reads its state word in its colour instead of the label.

import { useCameraHealthStore } from "@/hooks/use-camera-health";
import { cameraLabel } from "@/lib/feed-legs";
import { legCameraWord } from "@/lib/sensor-words";
import type { RosterCamera } from "@/lib/types";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { useFeedStore } from "@/stores/feed-store";

export function CameraSwitcher({ cameras }: { cameras: RosterCamera[] }) {
  const activeCameraId = useFeedStore((s) => s.activeCameraId);
  const setActiveCamera = useFeedStore((s) => s.setActiveCamera);
  const health = useCameraHealthStore((s) => s.cameras);

  // Default to the first camera when nothing is selected yet.
  const activeId = activeCameraId ?? cameras[0]?.id ?? null;

  return (
    <div role="group" aria-label={specText("cockpit.menu.cameras")} className="flex flex-wrap gap-[6px]" data-testid="menu-cameras">
      {cameras.map((cam) => {
        const active = cam.id === activeId;
        const label = cameraLabel(cam);
        const camWord = legCameraWord(health, cam, label);
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
            {camWord ? <span style={{ color: camWord.hex }}>{camWord.text}</span> : label}
          </button>
        );
      })}
    </div>
  );
}
