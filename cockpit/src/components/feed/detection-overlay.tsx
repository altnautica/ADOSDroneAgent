// L1 — the vision/detection overlay, drawn between the L0 WHEP video and the L2
// HUD. The vision engine runs on this same companion, so the cockpit paints its
// OWN live detections over the feed — the on-edge differentiator a remote ground
// control cannot render cheaply.
//
// Boxes arrive in the inference frame's pixel space; the video paints its stream
// with `object-contain`, so this overlay measures its own container (a
// ResizeObserver, identical to the video's `inset-0` box) and the video's natural
// resolution (published by the video layer), computes the letterboxed video
// rectangle, and places each box inside THAT rect — never a naive percentage of
// the panel, which would slide boxes into the letterbox bars.
//
// Honest surfaces (Rule 44): a track's colour + label reflect its real
// `lock_state` (green locked / amber uncertain / red lost) and `track_id`; an
// untracked detection falls back to a confidence ramp. Stale batches age out so a
// stopped feed does not pin the last frame's boxes on screen, and an empty batch
// draws nothing. The overlay is read-only (pointer-events pass through to the feed
// and the controls beneath it).

import { useEffect, useRef, useState } from "react";

import {
  DETECTION_STALE_MS,
  useDetectionsStore,
  type CockpitDetection,
} from "@/stores/detections-store";
import { useFeedStore } from "@/stores/feed-store";
import {
  computeRenderedRect,
  pickActiveBatch,
  scaleBoxToRect,
} from "@/lib/overlay-geometry";
import { cn } from "@/lib/utils";

interface DetectionOverlayProps {
  /** The active video leg id (from the roster), or null for the primary leg. */
  activeCameraId: string | null;
  /** Whether the node exposes more than one camera (drives leg correlation). */
  multiStream: boolean;
}

/** Border + text colour for a box. A track the engine reports a lock state for is
 *  coloured by that state; an untracked detection falls back to a confidence
 *  ramp. Uses the cockpit's semantic tokens. */
function boxColorClass(d: CockpitDetection): string {
  if (d.trackId != null && d.lockState) {
    if (d.lockState === "locked") return "border-ok text-ok";
    if (d.lockState === "uncertain") return "border-warn text-warn";
    return "border-err text-err"; // lost
  }
  if (d.confidence >= 0.7) return "border-amber text-amber";
  if (d.confidence >= 0.4) return "border-warn text-warn";
  return "border-muted-foreground text-muted-foreground";
}

/** The honest per-track label: class, track id when tracked, the lock state word
 *  when known, and the confidence percentage. */
function boxLabel(d: CockpitDetection): string {
  const pct = Math.round(d.confidence * 100);
  const parts: string[] = [d.classLabel || "object"];
  if (d.trackId != null) parts.push(`#${d.trackId}`);
  if (d.trackId != null && d.lockState) parts.push(d.lockState);
  parts.push(`${pct}%`);
  return parts.join(" · ");
}

export function DetectionOverlay({
  activeCameraId,
  multiStream,
}: DetectionOverlayProps) {
  const latest = useDetectionsStore((s) => s.latest);
  const byCamera = useDetectionsStore((s) => s.byCamera);
  // The video's decoded resolution, published off the real <video> element by the
  // video layer. Used as the letterbox source AR; falls back to the batch frame
  // size (the same camera source) when metadata is not in yet.
  const videoWidth = useFeedStore((s) => s.videoWidth);
  const videoHeight = useFeedStore((s) => s.videoHeight);

  const containerRef = useRef<HTMLDivElement | null>(null);
  const [size, setSize] = useState<{ w: number; h: number }>({ w: 0, h: 0 });

  // Re-measure the container on resize so the rendered rect tracks the panel.
  useEffect(() => {
    const el = containerRef.current;
    if (!el) return;
    const measure = () => {
      const r = el.getBoundingClientRect();
      setSize({ w: r.width, h: r.height });
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, []);

  const batch = pickActiveBatch(multiStream, latest, byCamera, activeCameraId);

  // A ticking clock so the staleness gate drops boxes once the feed stops, even
  // when no new batch arrives to re-render. Reading the clock from state (not
  // Date.now() in render) keeps the render pure.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!batch) return;
    const id = setInterval(() => setNow(Date.now()), 500);
    return () => clearInterval(id);
  }, [batch]);

  const fresh = batch != null && now - batch.receivedAt <= DETECTION_STALE_MS;

  const srcW = videoWidth ?? batch?.frameWidth ?? 0;
  const srcH = videoHeight ?? batch?.frameHeight ?? 0;
  const rect = computeRenderedRect(size.w, size.h, srcW, srcH);

  return (
    <div
      ref={containerRef}
      className="pointer-events-none absolute inset-0 z-[5]"
      aria-hidden
    >
      {fresh && batch
        ? batch.detections.map((d, i) => {
            // A box-less percept (mask/pose/depth-only) has no box to paint here.
            if (!d.bbox) return null;
            const placed = scaleBoxToRect(
              d.bbox,
              batch.frameWidth,
              batch.frameHeight,
              rect,
            );
            if (!placed || placed.width <= 0 || placed.height <= 0) return null;
            return (
              <div
                key={`${batch.frameId}-${i}`}
                className={cn("absolute border", boxColorClass(d))}
                style={{
                  left: `${placed.left}px`,
                  top: `${placed.top}px`,
                  width: `${placed.width}px`,
                  height: `${placed.height}px`,
                }}
              >
                <span className="absolute left-0 top-0 -translate-y-full whitespace-nowrap bg-background/80 px-[0.2rem] font-mono text-[0.6rem] leading-tight">
                  {boxLabel(d)}
                </span>
              </div>
            );
          })
        : null}
    </div>
  );
}
