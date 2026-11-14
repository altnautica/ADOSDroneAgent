// The picture-in-picture inset: a second video leg (the belly camera by
// default). A picture-in-picture never draws data layers or targets
// (`layers.json` `pip_draws_layers: false`); those stay on the main video.
// The inset plays through its own transport and keeps its snapshot local, so
// it never overwrites the main video's state in the feed store. Its header
// swaps the inset with the main video; the old main then stays in the inset.
// Sizes are CSS px; the grid's class scale applies around it.

import { specText } from "@/shared/hud/spec-text";
import { useState } from "react";
import { ArrowLeftRight } from "lucide-react";

import { VideoLayer } from "@/components/shell/video-layer";
import { cameraLabel, resolveLegVideoUrls } from "@/lib/feed-legs";
import { legCameraWord } from "@/lib/sensor-words";
import type { RosterCamera } from "@/lib/types";
import type { VideoTransportSnapshot } from "@/shared/video-transport";
import { useCameraHealthStore } from "@/hooks/use-camera-health";
import { useFeedStore } from "@/stores/feed-store";

/** Inset width: a fixed share of the stage, bounded for tiny and TV screens. */
const PIP_WIDTH = "clamp(140px, 22vw, 260px)";
/** Before the stream reports its size: the belly camera's 4:3. */
const DEFAULT_ASPECT = 4 / 3;

export function PipInset({ camera, streamNonce }: { camera: RosterCamera; streamNonce: number }) {
  const swapPip = useFeedStore((s) => s.swapPip);
  const [video, setVideo] = useState<VideoTransportSnapshot | null>(null);
  const { whepUrl, hlsUrl } = resolveLegVideoUrls(camera);
  const label = cameraLabel(camera);
  const health = useCameraHealthStore((s) => s.cameras);
  // A stale or lost camera names itself in its state colour; live adds nothing.
  const camWord = legCameraWord(health, camera, label);
  const aspect = video?.width && video.height ? video.width / video.height : DEFAULT_ASPECT;

  return (
    <div
      className="pointer-events-auto relative overflow-hidden rounded-lg border border-surface-foreground/20 bg-letterbox shadow-lg"
      style={{ width: PIP_WIDTH, aspectRatio: String(aspect) }}
      data-testid="pip-inset"
    >
      <VideoLayer
        whepUrl={whepUrl}
        hlsUrl={hlsUrl}
        reconnectKey={`pip:${camera.id}:${whepUrl}:${streamNonce}`}
        onSnapshot={setVideo}
      />
      <button
        type="button"
        onClick={swapPip}
        aria-label={specText("cockpit.pip.swap_leg", { leg: label })}
        className="absolute left-[4px] top-[4px] z-[16] flex min-h-[48px] min-w-[48px] items-center gap-[5px] rounded-md bg-hud-glass-strong px-[7px] font-semibold uppercase tracking-wide text-hud-ink"
        style={{ fontSize: 11 }}
      >
        <ArrowLeftRight className="h-[13px] w-[13px]" aria-hidden />
        {camWord ? (
          <span data-testid="pip-camera-state" style={{ color: camWord.hex }}>
            {camWord.text}
          </span>
        ) : (
          label
        )}
      </button>
    </div>
  );
}
