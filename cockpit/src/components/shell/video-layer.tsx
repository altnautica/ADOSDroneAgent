// L0 — the full-bleed video layer. Plays the active leg through the shared
// video transport (WHEP first, HLS as the fallback, a frame watchdog deciding
// what "live" and "frozen" mean) and mirrors its snapshot into the feed store
// so the status strip and alerts read the real element, never a guess.

import { useEffect, useRef } from "react";

import { useVideoTransport } from "@/shared/use-video-transport";
import { useFeedStore } from "@/stores/feed-store";

const ORDER: ("whep" | "hls")[] = ["whep", "hls"];

export function VideoLayer({
  whepUrl,
  hlsUrl,
  reconnectKey,
}: {
  /** WHEP endpoint for the active leg (`/whep`, or `/whep?camera=<id>`). */
  whepUrl: string;
  /** HLS endpoint for the same leg. */
  hlsUrl: string;
  /** Changing this tears the session down and dials again. */
  reconnectKey?: string;
}) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  const snapshot = useVideoTransport(videoRef, { order: ORDER, whepUrl, hlsUrl, reconnectKey });
  const setVideo = useFeedStore((s) => s.setVideo);

  useEffect(() => {
    setVideo(snapshot);
  }, [snapshot, setVideo]);
  useEffect(() => () => setVideo(null), [setVideo]);

  const message =
    snapshot.state === "connecting"
      ? "Connecting to feed…"
      : snapshot.state === "failed"
        ? "No video source"
        : null;

  return (
    <div className="absolute inset-0 z-0 bg-letterbox">
      <video
        ref={videoRef}
        className="h-full w-full object-contain"
        autoPlay
        muted
        playsInline
      />
      {message ? (
        <div className="absolute inset-0 flex items-center justify-center">
          <span className="rounded-md bg-hud-glass-strong px-[0.9rem] py-[0.5rem] text-[0.9rem] text-hud-ink-2">
            {message}
          </span>
        </div>
      ) : null}
    </div>
  );
}
