// A demo-mode navigation feed. Feeds the nav-stream store through the same
// `ingest` seam the live socket uses, so `?demo=1` exercises every HUD state
// with no agent attached: the follow phase cycles acquiring → following →
// coasting → searching → re-acquired → lost every 4 s on the demo detection
// stream's track 12; the vehicle circles the origin in the local frame with a
// matching SLAM pose and a planar scan of a 6 m room; vision-nav reports
// external nav; the map status climbs. Loaded only when `isDemoMode()`.

import { useNavStreamStore } from "@/stores/nav-stream-store";

const TICK_MS = 100;
const PHASE_MS = 4000;
/** The demo detection stream's camera and frame (see `mock-detections`). */
const CAMERA_ID = "demo-cam-0";
const FRAME_W = 640;
const FRAME_H = 480;
const TRACK_ID = 12;

const PHASES = ["acquiring", "following", "coasting", "searching", "following", "lost"] as const;

/** Ranges of a square room of half-width `half` seen from `(x, y)` with body
 *  yaw `yaw` (FRD, 1° bins clockwise from forward). */
function roomScan(x: number, y: number, yaw: number, half: number): number[] {
  const ranges: number[] = [];
  for (let i = 0; i < 360; i++) {
    const a = yaw + (i * Math.PI) / 180;
    const dx = Math.cos(a);
    const dy = Math.sin(a);
    const tx = dx > 0 ? (half - x) / dx : dx < 0 ? (-half - x) / dx : Infinity;
    const ty = dy > 0 ? (half - y) / dy : dy < 0 ? (-half - y) / dy : Infinity;
    const r = Math.min(tx, ty);
    ranges.push(r > 0.05 && r <= 12 ? Math.round(r * 100) / 100 : 0);
  }
  return ranges;
}

class MockNavStream {
  private timer: ReturnType<typeof setInterval> | null = null;
  private startMs = 0;
  private lastPhase = -1;

  start(): void {
    if (this.timer !== null) return;
    this.startMs = Date.now();
    this.lastPhase = -1;
    useNavStreamStore.getState().setLink("connected");
    this.timer = setInterval(() => this.tick(), TICK_MS);
  }

  stop(): void {
    if (this.timer !== null) clearInterval(this.timer);
    this.timer = null;
    useNavStreamStore.getState().clear();
  }

  private tick(): void {
    const store = useNavStreamStore.getState();
    const t = (Date.now() - this.startMs) / 1000;
    const index = Math.floor((t * 1000) / PHASE_MS) % PHASES.length;
    const phase = PHASES[index];
    if (index !== this.lastPhase) {
      if (index === 0) {
        store.designate({ cameraId: CAMERA_ID, trackId: TRACK_ID, bbox: { x: 0, y: 0, width: 0, height: 0 } });
      }
      if (index === 4) {
        store.ingest({ kind: "follow_event", data: { topic: "follow.reacquired", trackId: TRACK_ID, similarity: 0.86, gapS: 1.8 } });
      }
      if (index === 5) {
        store.ingest({ kind: "follow_event", data: { topic: "follow.lost", trackId: TRACK_ID, reason: "reacquire_timeout" } });
      }
      this.lastPhase = index;
    }

    // The vehicle circles the origin at 2 m, nose along the circle.
    const w = 0.2;
    const x = 2 * Math.cos(w * t);
    const y = 2 * Math.sin(w * t);
    const yaw = w * t + Math.PI / 2;
    const c = Math.cos(yaw);
    const s = Math.sin(yaw);
    const range = 9 + 2 * Math.sin(t * 0.3);
    const inPhase = ((t * 1000) % PHASE_MS) / 1000;

    store.ingest({
      kind: "pose",
      data: { R: [c, -s, 0, s, c, 0, 0, 0, 1], t: [x, y, -3], health: "good", frame: "map_frd", source: "lidar_planar" },
    });
    store.ingest({
      kind: "scan",
      data: { frame: "body_frd", angleStartRad: 0, angleIncrementRad: Math.PI / 180, ranges: roomScan(x, y, yaw, 6) },
    });
    store.ingest({
      kind: "nav",
      data: {
        mode: "external_nav",
        activeSet: 1,
        extnavHealth: "good",
        odomRateHz: 10,
        lastResetCounter: 0,
        obstacleRateHz: 10,
        minObstacleM: null,
        gpsFix: 0,
      },
    });
    store.ingest({
      kind: "follow",
      data: {
        active: phase !== "lost",
        phase,
        trackId: TRACK_ID,
        confidence: 0.9,
        rangeM: range,
        rangeSigmaM: 0.3,
        rangeCues: [
          { kind: "ground", rangeM: range + 0.1, sigmaM: 0.5 },
          { kind: "lidar", rangeM: range, sigmaM: 0.05 },
        ],
        targetSpeedMps: 1.4,
        aglM: 4.1,
        aglSource: "rng",
        reacquireMode: "smart",
        coastRemainingS: phase === "coasting" ? Math.max(0, 4 - inPhase) : null,
        pipelineLatencyMs: 85,
        holdReason: null,
        bearingDeg: ((yaw * 180) / Math.PI + 30 + 360) % 360,
        depressionDeg: 22,
        targetNed: [x + range * Math.cos(yaw), y + range * Math.sin(yaw)],
        targetVelNed: [1.4 * c, 1.4 * s],
        predictedBbox:
          phase === "coasting" || phase === "searching"
            ? { cameraId: CAMERA_ID, frameW: FRAME_W, frameH: FRAME_H, x: 300 + 40 * inPhase, y: 180, w: 60, h: 150 }
            : null,
      },
    });
    store.ingest({
      kind: "map",
      data: {
        sessionId: "demo",
        placement: "hybrid",
        memoryUsedMb: Math.min(700, 120 + t * 4),
        memoryBudgetMb: 614,
        voxelM: t > 120 ? 0.2 : 0.1,
        keyframesDropped: Math.floor(t / 30),
        sweepPoints: Math.floor(t * 1200),
        areaM2: Math.min(144, t),
        navPose: "always",
        upload: { state: "queued", bytesDone: 0, bytesTotal: 0 },
        stream: { state: "off", bytesPerS: 0 },
      },
    });
  }
}

/** Singleton demo nav stream. */
export const mockNavStream = new MockNavStream();
