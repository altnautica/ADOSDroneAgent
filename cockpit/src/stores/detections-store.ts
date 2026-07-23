// The on-box cockpit's live vision-detection state. The vision engine runs on
// this same companion, so the cockpit reads its OWN detections and paints boxes
// over the video — the on-edge differentiator a remote ground control cannot
// render cheaply. A single node's detections are kept here: the newest batch
// across every camera (the single-camera common case) plus the newest batch per
// camera id (so a multi-camera node can correlate boxes to the active leg and
// never paint one camera's boxes over another camera's video).
//
// A batch carries PIXEL-space boxes in the inference frame's own resolution
// (origin top-left); each batch declares that frame size so the overlay maps
// frame pixels onto the letterboxed video rectangle regardless of the panel's
// display size. The live WebSocket client (`@/lib/vision-detections-ws`) and the
// demo mock stream both feed this store through `setBatch`, so the overlay code
// path is identical whether the source is a real engine or the mock.

import { create } from "zustand";

/** How long a batch stays "fresh" after receipt. Past this the overlay drops
 *  its boxes so a stopped feed does not pin the last frame's boxes on screen.
 *  Matches the GCS overlay's staleness window. */
export const DETECTION_STALE_MS = 2000;

/** Discrete identity-lock state of a track this frame, mirroring the vision
 *  contract `LockState` (lowercase on the wire). `locked` = the tracker held the
 *  identity; `uncertain` = a weak/provisional association; `lost` = the track
 *  could not be re-associated. Carrying it lets the overlay show identity
 *  uncertainty honestly instead of hiding a silent swap. */
export type LockState = "locked" | "uncertain" | "lost";

/** A pixel-space bounding box (origin top-left) in the inference frame's own
 *  resolution. Mirrors the vision-contract `BoundingBox`. */
export interface DetectionBox {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** One percept the overlay can paint. The wire carries richer optional fields
 *  (mask, keypoints, depth, world position); the box overlay reads only the box
 *  plus the identity/confidence signal, so those extras are decoded-and-ignored
 *  rather than typed here. `bbox` is optional because a box-less percept (a
 *  mask/pose/depth-only reading) has no box to paint. */
export interface CockpitDetection {
  /** The 2D box in frame pixels. Absent for a box-less percept. */
  bbox?: DetectionBox;
  classLabel: string;
  confidence: number;
  /** Stable track id across frames (tracking models only). Absent for a
   *  stateless detector. */
  trackId?: number | null;
  /** Discrete identity-lock state this frame. Absent when the source reports
   *  none. */
  lockState?: LockState | null;
}

/** One frame's batch of detections, plus the frame size its boxes are expressed
 *  in and the epoch-ms receipt stamp used to age stale boxes out. */
export interface CockpitDetectionBatch {
  modelId: string;
  cameraId: string;
  frameId: number;
  tsMs: number;
  /** The inference frame resolution the boxes are expressed in. The overlay
   *  scales boxes by (renderedVideoRect / frame). */
  frameWidth: number;
  frameHeight: number;
  detections: CockpitDetection[];
  /** Epoch ms this cockpit received the batch, stamped by `setBatch`. */
  receivedAt: number;
}

interface DetectionsState {
  /** Newest batch across all cameras — the single-camera common case reads
   *  this directly. */
  latest: CockpitDetectionBatch | null;
  /** Newest batch per camera id, so a multi-camera node can pick the batch that
   *  belongs to the active video leg. */
  byCamera: Record<string, CockpitDetectionBatch>;
  /** Replace the latest batch (and its per-camera entry). `receivedAt` is
   *  stamped here so callers do not have to. */
  setBatch: (batch: Omit<CockpitDetectionBatch, "receivedAt">) => void;
  /** Drop all detections (on feed stop / cockpit leaving the flying view). */
  clear: () => void;
}

export const useDetectionsStore = create<DetectionsState>((set) => ({
  latest: null,
  byCamera: {},
  setBatch: (batch) =>
    set((state) => {
      const stamped: CockpitDetectionBatch = { ...batch, receivedAt: Date.now() };
      return {
        latest: stamped,
        byCamera: { ...state.byCamera, [stamped.cameraId]: stamped },
      };
    }),
  clear: () => set({ latest: null, byCamera: {} }),
}));
