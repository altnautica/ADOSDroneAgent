// Pure geometry for the detection overlay: map a pixel-space bounding box in the
// inference frame onto the LETTERBOXED video rectangle the panel actually shows.
//
// The video element paints its stream with `object-contain`, so on a panel whose
// aspect ratio differs from the stream's the video is letterboxed (bars on the
// sides or top/bottom). Positioning boxes as naive percentages of the whole panel
// would slide them off the video into the bars. Instead we compute the rendered
// video rectangle from the stream's intrinsic size vs the container size (the
// exact rect `object-contain` produces) and place each box inside THAT rect. All
// functions here are pure so the mapping is unit-testable without a DOM.

import type { CockpitDetectionBatch } from "@/stores/detections-store";

/** The rectangle (in container pixels, origin top-left) the video content
 *  actually occupies after `object-contain` letterboxing. */
export interface RenderedRect {
  left: number;
  top: number;
  width: number;
  height: number;
}

/** A positioned box in container pixels, ready to place `absolute`. */
export interface PlacedBox {
  left: number;
  top: number;
  width: number;
  height: number;
}

/**
 * The rectangle `object-contain` gives a `streamW x streamH` source inside a
 * `wrapperW x wrapperH` container: the source is scaled by the smaller of the
 * two axis ratios and centred, leaving equal letterbox bars on the constrained
 * axis. A non-positive dimension (metadata not in yet) degrades to the full
 * container so boxes still draw (un-letterboxed) rather than vanishing.
 */
export function computeRenderedRect(
  wrapperW: number,
  wrapperH: number,
  streamW: number,
  streamH: number,
): RenderedRect {
  if (streamW <= 0 || streamH <= 0 || wrapperW <= 0 || wrapperH <= 0) {
    return { left: 0, top: 0, width: Math.max(0, wrapperW), height: Math.max(0, wrapperH) };
  }
  const scale = Math.min(wrapperW / streamW, wrapperH / streamH);
  const width = streamW * scale;
  const height = streamH * scale;
  return {
    left: (wrapperW - width) / 2,
    top: (wrapperH - height) / 2,
    width,
    height,
  };
}

/**
 * Map a frame-pixel box onto `rect` (the rendered video rectangle). The box is
 * expressed as a fraction of the inference frame (`frameW x frameH`) — the same
 * source the video shows — so its fraction of the frame equals its fraction of
 * the rendered rect. The result is clamped inside the rect so a box that overruns
 * the frame edge does not paint into the letterbox bars. Returns `null` for a
 * degenerate frame size (nothing can be placed).
 */
export function scaleBoxToRect(
  box: { x: number; y: number; width: number; height: number },
  frameW: number,
  frameH: number,
  rect: RenderedRect,
): PlacedBox | null {
  if (frameW <= 0 || frameH <= 0) return null;
  const rawLeft = rect.left + (box.x / frameW) * rect.width;
  const rawTop = rect.top + (box.y / frameH) * rect.height;
  const rawWidth = (box.width / frameW) * rect.width;
  const rawHeight = (box.height / frameH) * rect.height;

  const rectRight = rect.left + rect.width;
  const rectBottom = rect.top + rect.height;
  const left = Math.max(rect.left, Math.min(rectRight, rawLeft));
  const top = Math.max(rect.top, Math.min(rectBottom, rawTop));
  const width = Math.max(0, Math.min(rectRight - left, rawWidth));
  const height = Math.max(0, Math.min(rectBottom - top, rawHeight));
  return { left, top, width, height };
}

/**
 * Pick the batch to draw over the active video leg. On a single-stream node the
 * newest batch (any camera) is used. On a MULTI-stream node the boxes must belong
 * to the leg on screen — otherwise a batch from another camera (e.g. an IR
 * tracker) would draw its boxes over the EO video after the operator switches
 * legs — so the batch whose camera id matches the active leg is used, and nothing
 * is drawn when the active leg has no detections (never another leg's boxes).
 */
export function pickActiveBatch(
  multiStream: boolean,
  latest: CockpitDetectionBatch | null,
  byCamera: Record<string, CockpitDetectionBatch>,
  activeCameraId: string | null,
): CockpitDetectionBatch | null {
  if (!multiStream) return latest;
  if (activeCameraId == null) return null;
  return byCamera[activeCameraId] ?? null;
}
