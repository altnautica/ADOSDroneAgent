// The picture-in-picture inset: a second video leg (the belly camera by
// default) with its own host target layer, pinned to that leg's detections
// (`pickActiveBatch` with the inset's camera id). The inset plays through its
// own transport and keeps its snapshot local, so it never overwrites the main
// video's state in the feed store. Its header swaps the inset with the main
// video; the old main then stays in the inset.

import { useState } from "react";
import { ArrowLeftRight } from "lucide-react";

import { TargetLayer } from "@/components/feed/target-layer";
import { VideoLayer } from "@/components/shell/video-layer";
import type { WidthClass } from "@/lib/cockpit-layout";
import { cameraLabel, resolveLegVideoUrls } from "@/lib/feed-legs";
import type { RosterCamera } from "@/lib/types";
import type { VideoTransportSnapshot } from "@/shared/video-transport";
import { useFeedStore } from "@/stores/feed-store";

/** Inset width: a fixed share of the stage, bounded for tiny and TV screens. */
const PIP_WIDTH = "clamp(140px, 22vw, 260px)";
/** Before the stream reports its size: the belly camera's 4:3. */
const DEFAULT_ASPECT = 4 / 3;

export function PipInset({
  camera,
  streamNonce,
  visionCapable,
  widthClass,
}: {
  camera: RosterCamera;
  streamNonce: number;
  visionCapable: boolean;
  widthClass: WidthClass;
}) {
  const swapPip = useFeedStore((s) => s.swapPip);
  const [video, setVideo] = useState<VideoTransportSnapshot | null>(null);
  const { whepUrl, hlsUrl } = resolveLegVideoUrls(camera);
  const label = cameraLabel(camera);
  const aspect = video?.width && video.height ? video.width / video.height : DEFAULT_ASPECT;

  return (
    <div
      className="pointer-events-auto relative overflow-hidden rounded-lg border border-surface-foreground/20 bg-letterbox shadow-lg"
      style={{ width: PIP_WIDTH, aspectRatio: String(aspect) }}
    >
      <VideoLayer
        whepUrl={whepUrl}
        hlsUrl={hlsUrl}
        reconnectKey={`pip:${camera.id}:${whepUrl}:${streamNonce}`}
        onSnapshot={setVideo}
      />
      {visionCapable && video ? (
        <TargetLayer
          cameraId={camera.id}
          multiStream
          videoState={video.state}
          videoWidth={video.width}
          videoHeight={video.height}
          widthClass={widthClass}
          cameraYawOffsetDeg={camera.role === "down" ? null : 0}
          pip
        />
      ) : null}
      <button
        type="button"
        onClick={swapPip}
        aria-label={`Swap ${label} into the main view`}
        className="absolute left-[0.25rem] top-[0.25rem] z-[16] flex min-h-[32px] items-center gap-[0.3rem] rounded-md bg-hud-glass-strong px-[0.45rem] text-[0.7rem] font-semibold uppercase tracking-wide text-hud-ink"
      >
        <ArrowLeftRight className="h-[0.8rem] w-[0.8rem]" aria-hidden />
        {label}
      </button>
    </div>
  );
}
