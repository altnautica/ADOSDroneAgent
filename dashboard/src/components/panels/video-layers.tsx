// The dashboard's read-only data layers over its video: the cockpit's shared
// layer engine at the Route level (AR route and motion items, obstacles),
// fed by polling `GET /api/v1/nav/status?view=main:<layers>` at 5 Hz while the
// panel shows live video and the page is visible. The `view` query is the
// polling client's view lease: each request renews it, so the agent computes
// these layers for the primary camera only while this panel is watching.
//
// No controls here: the dashboard is the setup surface; the cockpit is the
// flight surface where the layer level, views and toggles live.

import { useEffect, useRef } from "react";

import { apiFetch } from "@/shared/api-fetch";
import { parseArFrame, parseObstaclesFrame, parsePilotState } from "@/shared/hud/cockpit-frames";
import { LayerEngine, LayerFrameStore } from "@/shared/hud/layer-engine";

const POLL_MS = 200;
/** The primary camera's served id. */
const CAMERA_ID = "main";
const LAYERS = ["ar_route", "ar_motion", "obstacles"] as const;
const VIEW_QUERY = `${CAMERA_ID}:${LAYERS.join(",")}`;

interface StatusEntry {
  ts_ms: number;
  age_ms: number;
  data: unknown;
}

type NavStatus = Partial<Record<"ar" | "obstacles" | "pilot", StatusEntry | null>>;

export function VideoLayers({
  active,
  videoRef,
}: {
  /** True while the panel shows live video. */
  active: boolean;
  videoRef: React.RefObject<HTMLVideoElement | null>;
}) {
  const boxRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);

  useEffect(() => {
    const box = boxRef.current;
    const canvas = canvasRef.current;
    if (!active || !box || !canvas) return;
    const store = new LayerFrameStore();
    const engine = new LayerEngine(store, {
      cameraId: CAMERA_ID,
      layers: new Set(LAYERS),
      level: "route",
      videoW: 1280,
      videoH: 720,
      mission: null,
      avoid: null,
      showLoaded: true,
      depthRangeM: 0,
      depthMode: "solid",
      fpsCap: 0,
    });
    engine.attach(canvas);
    const ro = new ResizeObserver(() => {
      const r = box.getBoundingClientRect();
      engine.resize(r.width, r.height, window.devicePixelRatio || 1);
      const v = videoRef.current;
      if (v && v.videoWidth > 0) engine.configure({ videoW: v.videoWidth, videoH: v.videoHeight });
    });
    ro.observe(box);

    let lastAr = -1;
    let lastObst = -1;
    let lastMission = "";
    let inFlight: AbortController | null = null;
    const poll = () => {
      if (inFlight || document.visibilityState === "hidden") return;
      const ac = new AbortController();
      inFlight = ac;
      apiFetch<NavStatus>(`/api/v1/nav/status?view=${encodeURIComponent(VIEW_QUERY)}`, { signal: ac.signal })
        .then((body) => {
          const at = performance.now();
          const pilot = body.pilot ? parsePilotState(body.pilot.data) : null;
          const missionKey = `${pilot?.mission?.state ?? ""}|${pilot?.avoidState ?? ""}|${pilot?.armed ?? ""}|${pilot?.owner ?? ""}`;
          if (missionKey !== lastMission) {
            lastMission = missionKey;
            engine.configure({
              mission: pilot?.mission ?? null,
              avoid: pilot?.avoidState ?? null,
              showLoaded: !pilot || !pilot.armed || pilot.owner === "pilot" || pilot.owner === "fc_mode",
            });
          }
          if (body.ar && body.ar.ts_ms !== lastAr) {
            const f = parseArFrame(body.ar.data);
            if (f) {
              lastAr = body.ar.ts_ms;
              store.putAr(f, at - body.ar.age_ms);
            }
          }
          if (body.obstacles && body.obstacles.ts_ms !== lastObst) {
            const f = parseObstaclesFrame(body.obstacles.data);
            if (f) {
              lastObst = body.obstacles.ts_ms;
              store.putObstacles(f, at - body.obstacles.age_ms);
            }
          }
        })
        .catch(() => undefined)
        .finally(() => {
          if (inFlight === ac) inFlight = null;
        });
    };
    poll();
    const timer = setInterval(poll, POLL_MS);
    const onVis = () => (document.visibilityState === "hidden" ? engine.stop() : engine.start());
    document.addEventListener("visibilitychange", onVis);
    engine.start();
    return () => {
      clearInterval(timer);
      inFlight?.abort();
      document.removeEventListener("visibilitychange", onVis);
      ro.disconnect();
      engine.stop();
    };
  }, [active, videoRef]);

  return (
    <div ref={boxRef} className="pointer-events-none absolute inset-0" aria-hidden>
      <canvas ref={canvasRef} className="absolute inset-0 h-full w-full" />
    </div>
  );
}
