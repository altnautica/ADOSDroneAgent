// The `video.overlay` host props (07-video-overlay-contract §3): everything a
// sandboxed overlay frame needs to draw letterbox-correct marks without ever
// measuring the host DOM — the camera, the stream size, the rendered video
// rect, the frame time, the vehicle attitude and the latest fresh detection
// batch. Pushed as one `video.overlay.props` event at detection rate, on a
// resize or resolution change, and once when a batch ages out (§6); attitude
// rides along with each push rather than at the telemetry rate. The shape is
// Mission Control's (`lib/plugins/video-overlay-props.ts`) field for field,
// and the extension SDK's `ctx.videoOverlay.onProps` reads it, so one overlay
// bundle draws the same on both hosts. Attitude is `null` when the telemetry
// is not live (never a wings-level zero), and `frameAgeMs` is `null` because
// this cockpit has no frame-age estimator, so `attitudeAtFrameTime` is false.

import type { RenderedRect } from "@/lib/overlay-geometry";
import type { VehicleAttitude } from "@/lib/types";
import type { CockpitDetectionBatch } from "@/stores/detections-store";

/** The host → frame event that carries the props. */
export const VIDEO_OVERLAY_PROPS_EVENT = "video.overlay.props";
/** The slot capability carried on the event, so an overlay can check the source. */
export const VIDEO_OVERLAY_PROPS_CAPABILITY = "ui.slot.video-overlay";

const RAD_TO_DEG = 180 / Math.PI;

export interface VideoOverlayDetection {
  bbox: { x: number; y: number; width: number; height: number };
  classLabel: string;
  confidence: number;
  trackId: number | null;
  lockState: "locked" | "uncertain" | "lost" | null;
}

export interface VideoOverlayHostProps {
  droneId: string;
  cameraId: string;
  streamWidth: number;
  streamHeight: number;
  renderedRect: { left: number; top: number; width: number; height: number };
  /** GCS-clock epoch ms of the batch the payload is coalesced to (its
   *  `receivedAt`); 0 with no batch shown. */
  frameTimestampMs: number;
  /** Measured age of the frame on screen, or null when unknown. */
  frameAgeMs: number | null;
  /** Vehicle attitude in degrees, or null when unknown or not live. */
  attitude: { rollDeg: number; pitchDeg: number; yawDeg: number } | null;
  /** The attitude is the sample at the frame's capture time. */
  attitudeAtFrameTime: boolean;
  detections: {
    frameWidth: number;
    frameHeight: number;
    frameId: number;
    receivedAt: number;
    items: VideoOverlayDetection[];
  } | null;
}

export interface VideoOverlayInput {
  droneId: string;
  cameraId: string;
  /** The decoded video size, or null before metadata. */
  videoWidth: number | null;
  videoHeight: number | null;
  rendered: RenderedRect;
  /** The batch for this video's camera, already age-checked (null = stale). */
  batch: CockpitDetectionBatch | null;
  /** The latest attitude (radians), or null when the telemetry is not live. */
  attitude: VehicleAttitude | null | undefined;
}

/** Attitude in degrees, or null unless all three angles are known. */
function attitudeDeg(a: VehicleAttitude | null | undefined): VideoOverlayHostProps["attitude"] {
  const ok = (v: number | null | undefined): v is number => typeof v === "number" && Number.isFinite(v);
  if (!a || !ok(a.roll) || !ok(a.pitch) || !ok(a.yaw)) return null;
  return { rollDeg: a.roll * RAD_TO_DEG, pitchDeg: a.pitch * RAD_TO_DEG, yawDeg: a.yaw * RAD_TO_DEG };
}

const round = (v: number) => Math.round(v * 10) / 10;

/** Build the props. The stream size falls back to the batch frame size before
 *  the video reports its own; detections without a box are left out (an
 *  overlay can neither draw nor hit-test them). */
export function buildVideoOverlayProps(i: VideoOverlayInput): VideoOverlayHostProps {
  const b = i.batch;
  return {
    droneId: i.droneId,
    cameraId: i.cameraId,
    streamWidth: i.videoWidth ?? b?.frameWidth ?? 0,
    streamHeight: i.videoHeight ?? b?.frameHeight ?? 0,
    renderedRect: {
      left: round(i.rendered.left),
      top: round(i.rendered.top),
      width: round(i.rendered.width),
      height: round(i.rendered.height),
    },
    frameTimestampMs: b?.receivedAt ?? 0,
    frameAgeMs: null,
    attitude: attitudeDeg(i.attitude),
    attitudeAtFrameTime: false,
    detections: b
      ? {
          frameWidth: b.frameWidth,
          frameHeight: b.frameHeight,
          frameId: b.frameId,
          receivedAt: b.receivedAt,
          items: b.detections.flatMap((d) =>
            d.bbox
              ? [
                  {
                    bbox: { x: d.bbox.x, y: d.bbox.y, width: d.bbox.width, height: d.bbox.height },
                    classLabel: d.classLabel,
                    confidence: d.confidence,
                    trackId: d.trackId ?? null,
                    lockState: d.lockState ?? null,
                  },
                ]
              : [],
          ),
        }
      : null,
  };
}

/** What makes a push material (§6): a new batch or its ageing out, a resize,
 *  a resolution change, a camera switch, or the attitude going unknown (or
 *  coming back). A changing attitude alone never pushes. */
export function videoOverlayPushKey(p: VideoOverlayHostProps): string {
  const r = p.renderedRect;
  return [
    p.droneId,
    p.cameraId,
    p.streamWidth,
    p.streamHeight,
    r.left,
    r.top,
    r.width,
    r.height,
    p.detections ? `${p.detections.frameId}@${p.detections.receivedAt}` : "none",
    p.attitude ? "att" : "no-att",
  ].join("|");
}
