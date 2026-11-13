// The cockpit grid's right column (cockpit spec `layout.json`): shutter,
// photo/video switch, zoom, gimbal pad and camera settings.

import { useEffect, useState, type MouseEvent, type PointerEvent, type ReactNode } from "react";
import { Camera, Circle, ImageIcon, Minus, Plus, SlidersHorizontal, Square, Video } from "lucide-react";

import { cameraRoute, endZoomPresses, recordRoute, runCameraAction, useMavCameraStore, type CameraAction } from "@/lib/camera-actions";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { FLIGHT_PX, type RightItem } from "@/lib/flight-layout";
import type { RosterCamera } from "@/lib/types";
import { cn } from "@/lib/utils";
import { pressOrExplain } from "@/lib/zone-press";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useStatusStore } from "@/stores/status-store";

import { useCameraReason, useVideoLive } from "./camera-reason";
import { CameraSettingsSheet } from "./camera-settings-sheet";
import { GimbalPad } from "./gimbal-pad";
import { reprobeOnNoVideo, watchMavCameras } from "./mav-camera-refresh";
import { ZoneButton } from "./zone-button";

const refreshMav = () => void useMavCameraStore.getState().refresh();

/** Zoom in over zoom out, one pill. Without a camera extension's zoom the
 *  pill zooms the shown video digitally and names the level beside it. A
 *  vehicle camera zooms while a button is held (a tap or a key press zooms
 *  briefly), so each button presses on pointer-down and releases on
 *  pointer-up; a keyboard activation is a press and its release at once. */
function ZoomPill() {
  const inReason = useCameraReason("zoom_in");
  const outReason = useCameraReason("zoom_out");
  const extensions = useExtensionsStore((s) => s.extensions);
  const mav = useMavCameraStore((s) => s.cameras);
  const digital = cameraRoute("zoom_in", extensions, true, mav).kind === "local";
  const zoom = useCockpitViewStore((s) => s.digitalZoom);
  const level = digital ? specText("camera.digital_zoom", { n: zoom }) : null;
  const btn = "flex h-1/2 w-full items-center justify-center active:bg-white/10 aria-disabled:opacity-45";
  // A pill that goes away mid-hold ends the zoom it started.
  useEffect(() => endZoomPresses, []);
  const hold = (action: CameraAction, reason: string | null) => {
    const release = () => void runCameraAction(action, "release");
    return {
      onPointerDown: (e: PointerEvent<HTMLButtonElement>) => {
        if (e.button !== 0) return;
        e.currentTarget.setPointerCapture?.(e.pointerId);
        pressOrExplain(reason, () => void runCameraAction(action));
      },
      onPointerUp: release,
      onPointerCancel: release,
      onLostPointerCapture: release,
      onClick: (e: MouseEvent<HTMLButtonElement>) => {
        if (e.detail !== 0) return;
        pressOrExplain(reason, () => {
          void runCameraAction(action);
          release();
        });
      },
    };
  };
  const inLabel = specText("cockpit.zoom.in");
  const outLabel = specText("cockpit.zoom.out");
  return (
    <div
      className="chrome-panel relative flex flex-col !rounded-[var(--fs-radius-sm)]"
      style={{ width: "var(--cockpit-col)", height: FLIGHT_PX.zoomH }}
      role="group"
      aria-label={level ?? specText("cockpit.zoom")}
      data-zone="zoom"
    >
      <button type="button" className={btn} {...hold("zoom_in", inReason)} aria-disabled={inReason ? true : undefined} title={inReason ?? inLabel} aria-label={inReason ? `${inLabel}: ${inReason}` : inLabel}>
        <Plus className="h-[20px] w-[20px]" aria-hidden />
      </button>
      <span className="mx-[12px] h-px bg-[var(--fs-border)]" aria-hidden />
      <button type="button" className={btn} {...hold("zoom_out", outReason)} aria-disabled={outReason ? true : undefined} title={outReason ?? outLabel} aria-label={outReason ? `${outLabel}: ${outReason}` : outLabel}>
        <Minus className="h-[20px] w-[20px]" aria-hidden />
      </button>
      {level && zoom > 1 ? (
        <span
          className="chrome-panel chrome-label absolute right-full top-1/2 mr-[6px] -translate-y-1/2 whitespace-nowrap !rounded-[var(--fs-radius-sm)] px-[8px]"
          style={{ fontSize: 12, lineHeight: "24px" }}
          data-testid="digital-zoom"
        >
          {level}
        </span>
      ) : null}
    </div>
  );
}

/** The right column: shutter, photo/video switch, zoom, gimbal pad and the
 *  camera settings, as the layout fits them. Photo, zoom and recentre go
 *  through the camera chain (`lib/camera-actions`); video drives this node's
 *  recorder; the pad sends gimbal rates; settings open a sheet over the
 *  flight screen. */
export function RightColumn({ items, cameras }: { items: readonly RightItem[]; cameras: readonly RosterCamera[] }) {
  const mode = useCockpitUiStore((s) => s.shutterMode);
  const setMode = useCockpitUiStore((s) => s.setShutterMode);
  const recording = useStatusStore((s) => s.status?.recording === true || s.status?.video?.recording === true);
  const photoReason = useCameraReason("photo");
  const recordReason = recordRoute(useVideoLive()).reason;
  const [settings, setSettings] = useState(false);
  // The vehicle's camera list, so photo and zoom know a MAVLink camera: on
  // mount, when the FC connects, every 30 s, and on a Photo press refused
  // for want of video.
  useEffect(() => watchMavCameras(refreshMav, useStatusStore), []);
  const render = (item: RightItem): ReactNode => {
    switch (item) {
      case "shutter":
        return (
          <ZoneButton
            key={item}
            icon={mode === "photo" ? Camera : recording ? Square : Circle}
            label={specText(mode === "photo" ? "cockpit.camera.take_photo" : recording ? "cockpit.camera.stop_recording" : "cockpit.camera.start_recording")}
            testId="shutter"
            size="var(--cockpit-shutter)"
            tone={mode === "video" && recording ? "active" : "default"}
            round
            onPress={() => runCockpitAction(mode === "photo" ? "camera.photo" : "camera.record")}
            reason={mode === "photo" ? photoReason : recordReason}
            onRefused={mode === "photo" ? () => reprobeOnNoVideo(photoReason, specText("camera.no_video"), refreshMav) : undefined}
          />
        );
      case "photo_video_switch": {
        // One control: the whole switch toggles, so its hit area is the
        // touch floor whatever the segments' drawn size.
        const next = mode === "photo" ? "video" : "photo";
        return (
          <button
            key={item}
            type="button"
            role="switch"
            aria-checked={mode === "video"}
            aria-label={specText("cockpit.camera.shutter_mode")}
            title={specText(mode === "photo" ? "cockpit.camera.photo" : "cockpit.camera.video")}
            onClick={() => setMode(next)}
            className="flex shrink-0 items-center justify-center"
            style={{ width: FLIGHT_PX.modeW, height: FLIGHT_PX.modeH }}
            data-zone="photo_video_switch"
          >
            <span className="chrome-panel flex h-[36px] w-full overflow-hidden !rounded-[var(--fs-radius-sm)] p-[2px]" aria-hidden>
              {(["photo", "video"] as const).map((m) => (
                <span
                  key={m}
                  className={cn(
                    "flex h-full flex-1 items-center justify-center rounded-[3px] transition-colors duration-150",
                    mode === m ? "bg-white/90 text-black" : "chrome-dim",
                  )}
                >
                  {m === "photo" ? <ImageIcon className="h-[15px] w-[15px]" /> : <Video className="h-[15px] w-[15px]" />}
                </span>
              ))}
            </span>
          </button>
        );
      }
      case "zoom":
        return <ZoomPill key={item} />;
      case "gimbal_pad":
        return <GimbalPad key={item} />;
      case "camera_settings":
        return (
          <ZoneButton
            key={item}
            icon={SlidersHorizontal}
            label={specText("cockpit.camera.settings")}
            testId="camera-settings"
            onPress={() => setSettings(true)}
          />
        );
    }
  };
  return (
    <div
      className="pointer-events-auto flex flex-col items-center"
      style={{ gap: FLIGHT_PX.colGap }}
      role="group"
      aria-label={specText("skill.picker.tab.camera")}
      data-zone="right_column"
    >
      {items.map(render)}
      {settings ? <CameraSettingsSheet cameras={cameras} onClose={() => setSettings(false)} /> : null}
    </div>
  );
}
