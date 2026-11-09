// Extension surfaces on the Feed, each gated on the matching granted slot
// capability (CS-12), placed on the CS-1 z-ladder:
//   video.overlay  — a full-feed draw layer at L1 (z 10). It takes no input,
//                    so taps pass through to the video; the host target
//                    layer (z 12) and the instrument HUD frame (z 15) draw
//                    above it. Each frame receives the `video.overlay.props`
//                    host props (07 §3): camera, stream size, rendered rect,
//                    frame time, attitude and the fresh detection batch.
//   cockpit.widget — a small frame in its declared corner of the widget zone
//                    (z 20): the stage area below the inset row and above the
//                    bottom chrome, so a widget never covers either.
// `cockpit.panel` frames open from the quick menu instead.

import { useMemo, useRef } from "react";

import { PluginFrameHost, type HostEvent } from "@/components/plugins/plugin-overlay-host";
import { useElementSize } from "@/hooks/use-element-size";
import { useNow } from "@/hooks/use-now";
import { BOTTOM_CHROME_CLEARANCE_PX, Z, ZONE_TOP_CLEARANCE_PX } from "@/lib/cockpit-layout";
import { pluginLayerId } from "@/lib/cockpit-view";
import { mountablePanels, type ExtensionPanel, type InstalledExtension } from "@/lib/extensions";
import { computeRenderedRect, pickActiveBatch } from "@/lib/overlay-geometry";
import { cn } from "@/lib/utils";
import {
  buildVideoOverlayProps,
  VIDEO_OVERLAY_PROPS_CAPABILITY,
  VIDEO_OVERLAY_PROPS_EVENT,
  videoOverlayPushKey,
} from "@/lib/video-overlay-props";
import { DETECTION_STALE_MS, useDetectionsStore } from "@/stores/detections-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useFlightStore } from "@/stores/flight-store";
import { useStatusStore } from "@/stores/status-store";

const CORNER_CLASS: Record<string, string> = {
  "top-left": "left-0 top-0",
  "top-right": "right-0 top-0",
  "bottom-left": "bottom-0 left-0",
  "bottom-right": "bottom-0 right-0",
};

/** The host props for the overlays over one video, rebuilt only when a push
 *  is material (07 §6); attitude is sampled at each push. */
function useVideoOverlayEvent(
  size: { w: number; h: number },
  cameraId: string | null,
  multiStream: boolean,
  videoWidth: number | null,
  videoHeight: number | null,
): HostEvent {
  const latest = useDetectionsStore((s) => s.latest);
  const byCamera = useDetectionsStore((s) => s.byCamera);
  const droneId = useStatusStore((s) => s.status?.paired_drone?.device_id ?? "local");
  const picked = pickActiveBatch(multiStream, latest, byCamera, cameraId);
  // A clock only while a batch could still age out, so the stale push happens.
  const now = useNow(picked ? 500 : null);
  const fresh = picked && now - picked.receivedAt <= DETECTION_STALE_MS ? picked : null;
  const srcW = videoWidth ?? fresh?.frameWidth ?? 0;
  const srcH = videoHeight ?? fresh?.frameHeight ?? 0;
  // Attitude is sampled at each push; `live` re-renders so a dead link pushes
  // a null attitude rather than the last one.
  const live = useFlightStore((s) => s.live);
  const props = buildVideoOverlayProps({
    droneId,
    cameraId: cameraId ?? fresh?.cameraId ?? "main",
    videoWidth,
    videoHeight,
    rendered: computeRenderedRect(size.w, size.h, srcW, srcH),
    batch: fresh,
    attitude: live ? useFlightStore.getState().telemetry?.attitude : null,
  });
  const key = videoOverlayPushKey(props);
  // The push key is the material identity of `props` (07 §6): a changing
  // attitude alone never re-pushes.
  return useMemo(
    () => ({ method: VIDEO_OVERLAY_PROPS_EVENT, capability: VIDEO_OVERLAY_PROPS_CAPABILITY, args: props }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [key],
  );
}

/** The extensions' video overlays, filling the video they sit over. Each is
 *  the data layer `plugin:<plugin_id>:<overlay_id>` and draws only while that
 *  layer is in `layers` (its flag and the level: Full and Instruments draw
 *  them, Route and Off do not). */
export function ExtensionVideoOverlays({
  cameraId,
  multiStream,
  videoWidth,
  videoHeight,
  layers,
}: {
  cameraId: string | null;
  multiStream: boolean;
  videoWidth: number | null;
  videoHeight: number | null;
  layers: ReadonlySet<string>;
}) {
  const extensions = useExtensionsStore((s) => s.extensions);
  const overlays = extensions.flatMap((ext) =>
    mountablePanels(ext, "video.overlay")
      .filter((p) => layers.has(pluginLayerId(ext.pluginId, p.id)))
      .map((p) => ({ ext, p })),
  );
  if (overlays.length === 0) return null;
  return (
    <VideoOverlayStack
      overlays={overlays}
      cameraId={cameraId}
      multiStream={multiStream}
      videoWidth={videoWidth}
      videoHeight={videoHeight}
    />
  );
}

function VideoOverlayStack({
  overlays,
  cameraId,
  multiStream,
  videoWidth,
  videoHeight,
}: {
  overlays: { ext: InstalledExtension; p: ExtensionPanel }[];
  cameraId: string | null;
  multiStream: boolean;
  videoWidth: number | null;
  videoHeight: number | null;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const size = useElementSize(ref);
  const hostEvent = useVideoOverlayEvent(size, cameraId, multiStream, videoWidth, videoHeight);
  return (
    <div ref={ref} className={cn("pointer-events-none absolute inset-0", Z.extOverlay)}>
      {overlays.map(({ ext, p }) => (
        <div key={`${ext.pluginId}:${p.id}`} className="absolute inset-0">
          <PluginFrameHost ext={ext} title={p.title} className="h-full w-full" hostEvent={hostEvent} />
        </div>
      ))}
    </div>
  );
}

/** The extensions' cockpit widgets, in the widget zone of the stage they are
 *  placed in. `topClearancePx` keeps the zone below whatever sits at the top
 *  of the stage (the minimap inset row on the compact layout). */
export function ExtensionWidgetZones({ topClearancePx = ZONE_TOP_CLEARANCE_PX }: { topClearancePx?: number }) {
  const extensions = useExtensionsStore((s) => s.extensions);
  const widgets = extensions.flatMap((ext) => mountablePanels(ext, "cockpit.widget").map((p) => ({ ext, p })));
  if (widgets.length === 0) return null;
  return (
    <div
      className={cn("pointer-events-none absolute inset-x-0", Z.zones)}
      style={{ top: topClearancePx, bottom: BOTTOM_CHROME_CLEARANCE_PX }}
    >
      {widgets.map(({ ext, p }) => (
        <div
          key={`${ext.pluginId}:${p.id}`}
          className={cn(
            "pointer-events-auto absolute h-[9rem] w-[14rem] overflow-hidden rounded-xl border border-hud-hair bg-hud-glass backdrop-blur-hud",
            CORNER_CLASS[p.zone ?? ""] ?? CORNER_CLASS["bottom-left"],
          )}
        >
          <PluginFrameHost ext={ext} title={p.title} className="h-full w-full" />
        </div>
      ))}
    </div>
  );
}
