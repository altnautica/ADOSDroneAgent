// A demo-mode feed for the cockpit data layers: `pilot`, `ar`, `obstacles`,
// `depth` and `surfaces` frames at 20 Hz in the agent's wire shapes, pushed
// through the same parsers and stores the live nav stream uses, so `?demo=1`
// draws every layer with no agent attached. The scene: a four-waypoint
// mission flying toward WP4 with the route swaying as the vehicle yaws, a
// follow path detouring around an obstacle, an orbit ring, home off the left
// edge, and a wall closing in on the right of the frame.

import { ingestLayerFrame, parseNavFrame } from "@/lib/nav-stream";
import { useNavStreamStore } from "@/stores/nav-stream-store";

const TICK_MS = 50;
/** Every id the demo video can resolve to. */
const CAMERAS = ["main", "demo-cam-0"];
const COLS = 32;
const ROWS = 18;

function b64(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}

/** Grid code for a distance (inverse of the grid's log distance scale). */
function code(m: number): number {
  return Math.max(1, Math.min(255, Math.round(1 + (Math.log(m / 0.2) * 254) / Math.log(300))));
}

class MockLayerStream {
  private timer: number | null = null;
  private startMs = 0;
  private near = new Uint8Array(COLS * ROWS);
  private measured = new Uint8Array((COLS * ROWS) / 8);

  start(): void {
    if (this.timer !== null) return;
    this.startMs = Date.now();
    this.timer = window.setInterval(() => this.tick(), TICK_MS);
  }

  stop(): void {
    clearInterval(this.timer ?? undefined);
    this.timer = null;
  }

  private tick(): void {
    const t = (Date.now() - this.startMs) / 1000;
    const now = Date.now();
    const sway = 0.04 * Math.sin(t * 0.7);
    const seq = 4;
    const mission = { state: "active", seq, total: 6, item_reached: seq - 1, id: null, source: "mission_state", age_ms: 80, landing_seq: false };

    const pilot = parseNavFrame({
      kind: "pilot",
      data: {
        owner: "mission_fc",
        owner_label: null,
        skill: null,
        vehicle_class: "multirotor",
        control_mode: "ados",
        profile: "normal",
        avoid: "brake",
        fc_mode: 3,
        fc_mode_name: "AUTO",
        armed: true,
        landed: false,
        avoid_state: "detour",
        min_clearance_m: 2.4,
        plan_ned: [],
        readiness: [],
        mission,
      },
    });
    if (pilot) useNavStreamStore.getState().ingest(pilot);

    const items = [
      { t: "wp", k: "wp:4", seq: 4, role: "next", x: 0.52 + sway, y: 0.41, gx: 0.52 + sway, gy: 0.63, dist_m: Math.max(5, 120 - t * 2), eta_s: Math.max(0, 42 - t), size: 0.25 },
      { t: "wp", k: "wp:5", seq: 5, role: "after", x: 0.64 + sway, y: 0.37, gx: null, gy: null, dist_m: 180, eta_s: null, size: 0.18 },
      { t: "wp", k: "wp:6", seq: 6, role: "other", x: 0.74 + sway, y: 0.35, gx: null, gy: null, dist_m: 240, eta_s: null, size: 0.1 },
      { t: "leg", k: "leg:active", role: "active", pts: [[0.5, 0.95], [0.52 + sway, 0.63]] },
      { t: "leg", k: "leg:next", role: "next", pts: [[0.52 + sway, 0.63], [0.64 + sway, 0.6]] },
      { t: "leg", k: "leg:future", role: "future", pts: [[0.64 + sway, 0.6], [0.7 + sway, 0.58], [0.74 + sway, 0.57]] },
      { t: "fpv", k: "fpv", x: 0.5 + sway * 0.5, y: 0.55 },
      { t: "fpath", k: "fpath", state: "detour", pts: [[0.5, 0.72], [0.47, 0.66], [0.45 + sway, 0.6]] },
      { t: "fgoal", k: "fgoal", x: 0.45 + sway, y: 0.6 },
      {
        t: "orbit",
        k: "orbit",
        dir: "cw",
        cx: 0.3,
        cy: 0.68,
        pts: Array.from({ length: 24 }, (_, i) => {
          const a = (i / 24) * Math.PI * 2;
          return [0.3 + 0.1 * Math.cos(a), 0.68 + 0.04 * Math.sin(a)];
        }),
      },
      { t: "edge", k: "edge:home", of: "home", seq: null, ang: 2.6, dist_m: 35 },
    ];
    const cams: Record<string, unknown> = {};
    for (const id of CAMERAS) cams[id] = { frame_w: 1280, frame_h: 720, reason: null, items };
    ingestLayerFrame({ kind: "ar", data: { v: 1, ts_ms: now, approx: false, reason: null, mission, cameras: cams } });

    // A wall closing in on the right third of the frame, 1.5–4.5 m.
    this.near.fill(0);
    this.measured.fill(0);
    const wall = 3 + 1.5 * Math.sin(t * 0.5);
    for (let r = 4; r < ROWS - 2; r++) {
      for (let c = 22; c < COLS; c++) {
        const i = r * COLS + c;
        this.near[i] = code(wall + (COLS - c) * 0.05 + Math.abs(r - 9) * 0.04);
        if (c % 4 !== 0) this.measured[i >> 3] |= 1 << (i & 7);
      }
    }
    const grid = { cols: COLS, rows: ROWS, near: b64(this.near), measured: b64(this.measured) };
    const gridCams: Record<string, unknown> = {};
    for (const id of CAMERAS) gridCams[id] = grid;
    const sectors: (number | null)[] = Array.from({ length: 72 }, () => null);
    for (let s = 6; s < 14; s++) sectors[s] = wall + (s - 6) * 0.2;
    ingestLayerFrame({ kind: "obstacles", data: { v: 1, ts_ms: now, sectors_m: sectors, cameras: gridCams } });

    const depthCams: Record<string, unknown> = {};
    for (const id of CAMERAS) depthCams[id] = { ...grid, scale_source: "lidar" };
    ingestLayerFrame({ kind: "depth", data: { v: 1, ts_ms: now, scale_source: "lidar", cameras: depthCams } });
    for (const id of CAMERAS) {
      ingestLayerFrame({
        kind: "surfaces",
        data: { v: 1, ts_ms: now, camera_id: id, zones: [{ pts: [[0.18, 0.8], [0.34, 0.8], [0.36, 0.92], [0.16, 0.92]], score: 0.82, slope_deg: 3 }] },
      });
    }
  }
}

export const mockLayerStream = new MockLayerStream();
