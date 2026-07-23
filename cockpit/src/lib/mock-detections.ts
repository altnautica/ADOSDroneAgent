// A demo-mode synthetic detection generator. Pushes realistic per-frame batches
// into the detections store at inference cadence (~10 Hz) so the overlay is fully
// explorable with `?demo=1` and no agent/camera attached. The store's `setBatch`
// is the same seam the live WebSocket client feeds, so the overlay code path is
// identical to production. Loaded only when `isDemoMode()` is true; it produces
// complete, well-formed data (not a stub).
//
// It shows 1-3 tracked "person" boxes drifting across the frame on their own slow
// walks, each with a stable track id, and cycles each box's lock state
// (locked -> uncertain -> lost) so the overlay's green/amber/red colour ramp and
// the identity-uncertainty labels are exercised over time.

import type { CockpitDetection, LockState } from "@/stores/detections-store";
import { useDetectionsStore } from "@/stores/detections-store";

/** Frame the synthetic boxes are expressed in (a common 4:3 inference size). */
const FRAME_WIDTH = 640;
const FRAME_HEIGHT = 480;

/** Push rate (ms) — ~10 Hz, matching a typical on-companion inference loop. */
const TICK_MS = 100;

/** A synthetic camera id (a single-camera demo node). */
const CAMERA_ID = "demo-cam-0";

interface MockTrack {
  trackId: number;
  classLabel: string;
  w: number;
  h: number;
  cx0: number;
  cy0: number;
  ax: number;
  ay: number;
  wx: number;
  wy: number;
  phase: number;
  lockPhase: number;
}

const TRACKS: MockTrack[] = [
  { trackId: 7, classLabel: "person", w: 90, h: 190, cx0: 200, cy0: 250, ax: 110, ay: 40, wx: 0.18, wy: 0.32, phase: 0, lockPhase: 0 },
  { trackId: 12, classLabel: "person", w: 80, h: 170, cx0: 430, cy0: 240, ax: 90, ay: 60, wx: 0.24, wy: 0.2, phase: 1.7, lockPhase: 2.1 },
  { trackId: 21, classLabel: "person", w: 70, h: 150, cx0: 320, cy0: 300, ax: 130, ay: 30, wx: 0.14, wy: 0.27, phase: 3.4, lockPhase: 4.3 },
];

/** Cycle a lock state on a slow period so the overlay colour ramp animates: ~5s
 *  locked, ~1.5s uncertain, brief lost, back to locked. */
function lockStateAt(tSec: number, offset: number): LockState {
  const p = (tSec + offset) % 7;
  if (p < 5) return "locked";
  if (p < 6.5) return "uncertain";
  return "lost";
}

function clamp(v: number, lo: number, hi: number): number {
  return Math.max(lo, Math.min(hi, v));
}

class MockDetectionStream {
  private intervalId: ReturnType<typeof setInterval> | null = null;
  private startMs = 0;
  private frameId = 0;

  start(): void {
    if (this.intervalId !== null) return;
    this.startMs = Date.now();
    this.frameId = 0;
    this.intervalId = setInterval(() => this.tick(), TICK_MS);
  }

  stop(): void {
    if (this.intervalId !== null) {
      clearInterval(this.intervalId);
      this.intervalId = null;
    }
    useDetectionsStore.getState().clear();
  }

  isRunning(): boolean {
    return this.intervalId !== null;
  }

  private tick(): void {
    const tSec = (Date.now() - this.startMs) / 1000;
    // Breathe the visible count 1 -> 2 -> 3 -> 2 -> 1 so tracks appear/vanish.
    const visible = 1 + Math.round(1 + Math.sin(tSec * 0.25)); // 1..3

    const detections: CockpitDetection[] = [];
    for (let i = 0; i < Math.min(visible, TRACKS.length); i++) {
      const tr = TRACKS[i];
      const cx = tr.cx0 + tr.ax * Math.sin(tSec * tr.wx + tr.phase);
      const cy = tr.cy0 + tr.ay * Math.sin(tSec * tr.wy + tr.phase * 1.3);
      const x = clamp(cx - tr.w / 2, 0, FRAME_WIDTH - tr.w);
      const y = clamp(cy - tr.h / 2, 0, FRAME_HEIGHT - tr.h);
      const lock = lockStateAt(tSec, tr.lockPhase);
      const conf = lock === "locked" ? 0.9 : lock === "uncertain" ? 0.62 : 0.38;
      detections.push({
        bbox: { x, y, width: tr.w, height: tr.h },
        classLabel: tr.classLabel,
        confidence: conf,
        trackId: tr.trackId,
        lockState: lock,
      });
    }

    useDetectionsStore.getState().setBatch({
      modelId: "demo-yolov8n",
      cameraId: CAMERA_ID,
      frameId: this.frameId++,
      tsMs: Date.now(),
      frameWidth: FRAME_WIDTH,
      frameHeight: FRAME_HEIGHT,
      detections,
    });
  }
}

/** Singleton demo detection stream. */
export const mockDetectionStream = new MockDetectionStream();
