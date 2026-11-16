// The detection boxes over the dashboard video: the vision engine's newest
// batch for the primary camera (`GET /api/vision/detections/latest`), placed in
// the letterboxed video rect and drawn in the cockpit's corner-bracket style.

import { bracketArm, bracketPath } from "@/shared/hud/target-state";

export interface VideoBox {
  key: string;
  /** Corner-bracket SVG path in panel px. */
  path: string;
  label: string;
  /** Tag anchor (the box's top-left), panel px. */
  x: number;
  y: number;
  locked: boolean;
}

export interface DetectionFrame {
  frameW: number;
  frameH: number;
  boxes: { x: number; y: number; w: number; h: number; label: string; trackId: number | null; locked: boolean }[];
  /** Capture stamp of the batch (host monotonic ms). */
  tsMs: number;
  /** Set only on a crop-only batch: its boxes cover just this window. */
  cropWindow: { x: number; y: number; w: number; h: number } | null;
  /** With a crop-only batch, the camera's newest whole-frame batch the reply
   *  carried (`full_frame`), so a poller holds the boxes outside the crop. */
  fullFrame?: DetectionFrame | null;
}

const DEFAULT_FRAME_W = 640;
const DEFAULT_FRAME_H = 480;

const num = (v: unknown): number | null => (typeof v === "number" && Number.isFinite(v) ? v : null);

/** The batch a `/latest` reply carries, or null when it carries none (vision
 *  off, a stale batch, or another camera's). */
export function parseLatestDetections(body: unknown, cameraId: string): DetectionFrame | null {
  if (!body || typeof body !== "object") return null;
  const b = body as Record<string, unknown>;
  if (b.available !== true || b.stale === true || !Array.isArray(b.detections)) return null;
  if (typeof b.camera_id === "string" && b.camera_id !== cameraId) return null;
  const boxes: DetectionFrame["boxes"] = [];
  for (const raw of b.detections) {
    const d = raw && typeof raw === "object" ? (raw as Record<string, unknown>) : null;
    const bb = d?.bbox && typeof d.bbox === "object" ? (d.bbox as Record<string, unknown>) : null;
    if (!d || !bb) continue;
    const x = num(bb.x);
    const y = num(bb.y);
    const w = num(bb.width);
    const h = num(bb.height);
    if (x === null || y === null || w === null || h === null || w <= 0 || h <= 0) continue;
    const conf = num(d.confidence);
    const cls = typeof d.class_label === "string" && d.class_label ? d.class_label : "object";
    boxes.push({
      x,
      y,
      w,
      h,
      label: conf === null ? cls : `${cls} ${Math.round(conf * 100)}%`,
      trackId: num(d.track_id),
      locked: d.lock_state === "locked",
    });
  }
  const cw = b.crop_window && typeof b.crop_window === "object" ? (b.crop_window as Record<string, unknown>) : null;
  const [cx, cy, cwW, cwH] = [num(cw?.x), num(cw?.y), num(cw?.width), num(cw?.height)];
  return {
    frameW: num(b.frame_width) || DEFAULT_FRAME_W,
    frameH: num(b.frame_height) || DEFAULT_FRAME_H,
    tsMs: num(b.ts_ms) ?? 0,
    cropWindow: cx !== null && cy !== null && cwW !== null && cwH !== null ? { x: cx, y: cy, w: cwW, h: cwH } : null,
    boxes,
    fullFrame:
      b.full_frame && typeof b.full_frame === "object"
        ? parseLatestDetections({ ...(b.full_frame as Record<string, unknown>), available: true }, cameraId)
        : null,
  };
}

/** How long a whole-frame batch's boxes are held outside later crop windows. */
export const CROP_HOLD_MS = 1000;

/** The frame to draw for `f`, given the camera's latest whole-frame batch
 *  `full`: a crop-only frame adds `full`'s boxes whose centre lies outside the
 *  window, minus track ids `f` reports, while `full` is at most
 *  `CROP_HOLD_MS` older. A frame without a crop window replaces everything. */
export function holdOutsideCrop(full: DetectionFrame | null, f: DetectionFrame): DetectionFrame {
  const win = f.cropWindow;
  if (!win || !full || f.tsMs - full.tsMs > CROP_HOLD_MS) return f;
  const seen = new Set(f.boxes.flatMap((b) => (b.trackId !== null ? [b.trackId] : [])));
  const held = full.boxes.filter((b) => {
    if (b.trackId !== null && seen.has(b.trackId)) return false;
    const mx = b.x + b.w / 2;
    const my = b.y + b.h / 2;
    return mx < win.x || mx > win.x + win.w || my < win.y || my > win.y + win.h;
  });
  return { ...f, boxes: [...f.boxes, ...held] };
}

/** The boxes in panel px for a `object-contain` video of `videoW`×`videoH`
 *  (the decoded size; the frame size when unknown) in a `panelW`×`panelH` box. */
export function layoutVideoBoxes(
  f: DetectionFrame,
  panelW: number,
  panelH: number,
  videoW: number,
  videoH: number,
): VideoBox[] {
  const vw = videoW > 0 ? videoW : f.frameW;
  const vh = videoH > 0 ? videoH : f.frameH;
  if (panelW <= 0 || panelH <= 0) return [];
  const scale = Math.min(panelW / vw, panelH / vh);
  const left = (panelW - vw * scale) / 2;
  const top = (panelH - vh * scale) / 2;
  // Detections are in the inference frame's pixels, which span the video.
  const sx = (vw * scale) / f.frameW;
  const sy = (vh * scale) / f.frameH;
  return f.boxes.map((b, i) => {
    const x = left + b.x * sx;
    const y = top + b.y * sy;
    const w = b.w * sx;
    const h = b.h * sy;
    return {
      key: b.trackId !== null ? `t${b.trackId}` : `i${i}`,
      path: bracketPath(x, y, w, h, bracketArm(w, h)),
      label: b.trackId !== null ? `${b.label} #${b.trackId}` : b.label,
      x,
      y,
      locked: b.locked,
    };
  });
}

/** The page-visibility surface the lease keeper reads (the `document`). */
export interface VisibilitySource {
  readonly visibilityState: DocumentVisibilityState;
  addEventListener(type: "visibilitychange", fn: () => void): void;
  removeEventListener(type: "visibilitychange", fn: () => void): void;
}

/** Hold a detection lease while the page is visible: `demand(holdS)` at once
 *  and every `renewMs`, released with `demand(0)` as soon as the page is
 *  hidden (a background tab must not keep the drone's detector running) and
 *  taken again when it shows. Returns the stop, which releases too. */
export function holdDetectionLease(
  demand: (holdS: number) => void,
  doc: VisibilitySource,
  holdS: number,
  renewMs: number,
): () => void {
  let timer: ReturnType<typeof setInterval> | null = null;
  const take = () => {
    if (timer !== null) return;
    demand(holdS);
    timer = setInterval(() => demand(holdS), renewMs);
  };
  const release = () => {
    if (timer === null) return;
    clearInterval(timer);
    timer = null;
    demand(0);
  };
  const onVisibility = () => (doc.visibilityState === "hidden" ? release() : take());
  doc.addEventListener("visibilitychange", onVisibility);
  onVisibility();
  return () => {
    doc.removeEventListener("visibilitychange", onVisibility);
    release();
  };
}
