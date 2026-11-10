import { useEffect } from "react";
import { Circle, Square } from "lucide-react";

import { Sheet } from "@/components/ui/sheet";
import { recordRoute, STREAM_QUALITIES, useStreamQualityStore } from "@/lib/camera-actions";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { cameraLabel } from "@/lib/feed-legs";
import type { RosterCamera } from "@/lib/types";
import { cn } from "@/lib/utils";
import { pressOrExplain } from "@/lib/zone-press";
import { specText } from "@/shared/hud/spec-text";
import { useFeedStore } from "@/stores/feed-store";
import { useStatusStore } from "@/stores/status-store";

import { useVideoLive } from "./camera-reason";

/** The camera settings over the flight screen: which camera the main video
 *  shows, the stream quality (where the node serves a video profile) and the
 *  recorder. */
export function CameraSettingsSheet({ cameras, onClose }: { cameras: readonly RosterCamera[]; onClose: () => void }) {
  const active = useFeedStore((s) => s.mainCameraId);
  const setActive = useFeedStore((s) => s.setActiveCamera);
  const recording = useStatusStore((s) => s.status?.recording === true || s.status?.video?.recording === true);
  const recordReason = recordRoute(useVideoLive()).reason;
  const quality = useStreamQualityStore((s) => s.quality);
  const setQuality = useStreamQualityStore((s) => s.set);
  useEffect(() => {
    void useStreamQualityStore.getState().load();
  }, []);
  return (
    <Sheet title={specText("cockpit.camera.settings")} onDismiss={onClose} closeLabel={specText("cockpit.close")}>
      <div className="flex flex-col gap-[16px] px-[14px] pb-[14px]" data-testid="camera-settings-sheet">
        {cameras.length > 1 ? (
          <section role="radiogroup" aria-label={specText("cockpit.menu.cameras")} className="flex flex-col gap-[6px]">
            <h3 className="text-[13px] font-semibold text-muted-foreground">{specText("cockpit.menu.cameras")}</h3>
            {cameras.map((c) => (
              <button
                key={c.id}
                type="button"
                role="radio"
                aria-checked={c.id === active}
                onClick={() => setActive(c.id)}
                className={cn(
                  "flex min-h-[48px] items-center rounded-lg border px-[12px] text-left text-[15px]",
                  c.id === active ? "border-primary bg-primary/10" : "border-border",
                )}
              >
                {cameraLabel(c)}
              </button>
            ))}
          </section>
        ) : null}
        {quality ? (
          <section role="radiogroup" aria-label={specText("camera.quality")} className="flex flex-col gap-[6px]" data-testid="stream-quality">
            <h3 className="text-[13px] font-semibold text-muted-foreground">{specText("camera.quality")}</h3>
            <div className="flex gap-[6px]">
              {STREAM_QUALITIES.map((q) => (
                <button
                  key={q}
                  type="button"
                  role="radio"
                  aria-checked={q === quality}
                  onClick={() => void setQuality(q)}
                  className={cn(
                    "flex min-h-[48px] flex-1 items-center justify-center rounded-lg border px-[12px] text-[15px]",
                    q === quality ? "border-primary bg-primary/10" : "border-border",
                  )}
                >
                  {specText(`camera.quality.${q}`)}
                </button>
              ))}
            </div>
          </section>
        ) : null}
        <button
          type="button"
          onClick={() => pressOrExplain(recordReason, () => runCockpitAction("camera.record"))}
          aria-disabled={recordReason ? true : undefined}
          title={recordReason ?? undefined}
          className="flex min-h-[48px] items-center justify-center gap-[8px] rounded-lg border border-border text-[15px] font-semibold aria-disabled:opacity-45"
        >
          {recording ? <Square className="h-[18px] w-[18px]" aria-hidden /> : <Circle className="h-[18px] w-[18px] text-err" aria-hidden />}
          {specText(recording ? "cockpit.camera.stop_recording" : "cockpit.camera.start_recording")}
        </button>
      </div>
    </Sheet>
  );
}
