// L0 — the full-bleed video layer. Plays the active leg through the shared
// video transport (WHEP first, HLS as the fallback, a frame watchdog deciding
// what "live" and "frozen" mean) and mirrors its snapshot into the feed store
// so the status strip and alerts read the real element, never a guess. While
// no picture plays, a note says why, below the centre marks (never under the
// reticle or the horizon mark), in the spec's words and never an exception's
// text.

import { useEffect, useRef, useState } from "react";

import { useWidthClass } from "@/hooks/use-width-class";
import { classScale, VIDEO_NOTE_TOP, videoNote } from "@/lib/flight-layout";
import { specText } from "@/shared/hud/spec-text";
import { useVideoTransport } from "@/shared/use-video-transport";
import type { VideoTransportSnapshot } from "@/shared/video-transport";
import { useFeedStore } from "@/stores/feed-store";

const ORDER: ("whep" | "hls")[] = ["whep", "hls"];

export function VideoLayer({
  whepUrl,
  hlsUrl,
  reconnectKey,
  onSnapshot,
}: {
  /** WHEP endpoint for the active leg (`/whep`, or `/whep?camera=<id>`). */
  whepUrl: string;
  /** HLS endpoint for the same leg. */
  hlsUrl: string;
  /** Changing this tears the session down and dials again. */
  reconnectKey?: string;
  /** A secondary instance (the picture-in-picture inset) receives its
   *  snapshot here instead, so it never overwrites the main video's state in
   *  the feed store. */
  onSnapshot?: (snapshot: VideoTransportSnapshot) => void;
}) {
  const videoRef = useRef<HTMLVideoElement | null>(null);
  // A press on the note's Retry dials again.
  const [retries, setRetries] = useState(0);
  const snapshot = useVideoTransport(videoRef, { order: ORDER, whepUrl, hlsUrl, reconnectKey: `${reconnectKey ?? ""}#${retries}` });
  const setVideo = useFeedStore((s) => s.setVideo);
  const ownsStore = onSnapshot === undefined;

  useEffect(() => {
    if (onSnapshot) onSnapshot(snapshot);
    else setVideo(snapshot);
  }, [snapshot, setVideo, onSnapshot]);
  useEffect(() => (ownsStore ? () => setVideo(null) : undefined), [ownsStore, setVideo]);

  const k = classScale(useWidthClass().widthClass);

  const note = videoNote(snapshot.state, whepUrl !== "" || hlsUrl !== "");

  return (
    <div className="absolute inset-0 z-0 bg-letterbox">
      <video
        ref={videoRef}
        className="h-full w-full object-contain"
        autoPlay
        muted
        playsInline
      />
      {note ? (
        // The inset has no centre marks: its note stays centred.
        <div
          className={ownsStore ? "absolute inset-x-0 flex justify-center" : "absolute inset-0 flex items-center justify-center"}
          style={ownsStore ? { top: VIDEO_NOTE_TOP } : undefined}
          data-testid="video-note"
        >
          <span
            className="pointer-events-auto flex items-center rounded-md bg-hud-glass-strong text-hud-ink-2"
            style={{ fontSize: `${14 * k}px`, lineHeight: `${18 * k}px`, padding: `${8 * k}px ${14 * k}px`, gap: `${10 * k}px` }}
          >
            {note.text}
            {note.retry ? (
              <button
                type="button"
                onClick={() => setRetries((n) => n + 1)}
                className="rounded-md border border-hud-hair font-semibold text-hud-ink"
                style={{ minHeight: 48, padding: `0 ${12 * k}px` }}
                data-testid="video-retry"
              >
                {specText("video.state.retry")}
              </button>
            ) : null}
          </span>
        </div>
      ) : null}
    </div>
  );
}
