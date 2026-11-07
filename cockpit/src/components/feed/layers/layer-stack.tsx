// The cockpit data layers over one video, drawn into a single canvas between
// the target layer (z 12) and the instrument HUD (z 15): depth, surfaces,
// obstacles, then the AR route and motion items, in the cockpit-spec z order.
//
// Frames never pass through React: the nav stream (or the demo feed) writes
// them into the mutable `layerFrames` store and the shared `LayerEngine`
// repaints from it in a change-driven requestAnimationFrame loop, with keyed
// interpolation between the two newest AR frames. This component re-renders
// only when the camera, the layer set, the level, the pilot's mission/avoid
// state or the video size changes.
//
// Leases (the agent computes layers only for what is watched):
// - the nav-stream view lease names the on-screen camera and its enabled
//   layers, re-sent every 2 s, dropped when the page is hidden or unmounted;
// - depth and surfaces each hold a vision-engine demand lease
//   (`POST /api/vision/demand {kind, camera_id, hold_s: 6}` every 2 s).
// The loop stops while the page is hidden. The TV / kiosk class paints at
// most 30 frames a second, since the kiosk's GPU also decodes the video.

import { useEffect, useMemo, useRef, useState } from "react";

import type { WidthClass } from "@/lib/cockpit-layout";
import { effectiveLayers } from "@/lib/cockpit-view";
import { layerFrames } from "@/lib/layer-frames";
import { setViewLease } from "@/lib/nav-stream";
import { isVideoLost } from "@/lib/target-overlay";
import { isDemoMode } from "@/lib/utils";
import { apiFetch } from "@/shared/api-fetch";
import { LayerEngine, type PaintStats } from "@/shared/hud/layer-engine";
import type { VideoFeedState } from "@/shared/video-transport";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

/** Layers the agent computes per camera, named in the view lease. */
const LEASED_LAYERS = ["ar_route", "ar_motion", "obstacles", "depth", "surfaces"] as const;
/** Layers that also need a vision-engine demand lease. */
const DEMAND_LAYERS = ["depth", "surfaces"] as const;
const DEMAND_HOLD_S = 6;
const DEMAND_RENEW_MS = 2000;
/** The primary video leg's served id: the camera when no roster names one. */
const PRIMARY_CAMERA_ID = "main";
/** Paint cap on the TV / kiosk class. */
const KIOSK_FPS = 30;

declare global {
  interface Window {
    /** Paint timings of the mounted LayerStack, for the overlay budget checks. */
    __adosLayerStats?: () => PaintStats;
  }
}

export interface LayerStackProps {
  cameraId: string | null;
  /** Decoded video size; null before the first frame. */
  videoWidth: number | null;
  videoHeight: number | null;
  videoState: VideoFeedState;
  widthClass: WidthClass;
}

/** True while the page is visible. */
function usePageVisible(): boolean {
  const [visible, setVisible] = useState(() => typeof document === "undefined" || document.visibilityState !== "hidden");
  useEffect(() => {
    const on = () => setVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", on);
    return () => document.removeEventListener("visibilitychange", on);
  }, []);
  return visible;
}

/** Hold a vision demand lease of `kind` on `cameraId` while `active`. */
function useVisionDemand(kind: "depth" | "surfaces", cameraId: string | null, active: boolean): void {
  useEffect(() => {
    if (!active || cameraId === null || isDemoMode()) return;
    const send = (holdS: number) =>
      apiFetch("/api/vision/demand", {
        method: "POST",
        body: { kind, camera_id: cameraId, hold_s: holdS, owner: "cockpit" },
      }).catch(() => undefined);
    void send(DEMAND_HOLD_S);
    const timer = setInterval(() => void send(DEMAND_HOLD_S), DEMAND_RENEW_MS);
    return () => {
      clearInterval(timer);
      // Best effort: an unanswered release still lapses after DEMAND_HOLD_S.
      void send(0);
    };
  }, [kind, cameraId, active]);
}

export function LayerStack({ cameraId: rosterCameraId, videoWidth, videoHeight, videoState, widthClass }: LayerStackProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const boxRef = useRef<HTMLDivElement | null>(null);
  const engineRef = useRef<LayerEngine | null>(null);
  const cameraId = rosterCameraId ?? PRIMARY_CAMERA_ID;

  const view = useCockpitViewStore((s) => s.view);
  const level = useCockpitViewStore((s) => s.level);
  const flags = useCockpitViewStore((s) => s.layers);
  const cycleLevel = useCockpitViewStore((s) => s.cycleLevel);
  const depthRangeM = useCockpitViewStore((s) => s.depthRangeM);
  const depthMode = useCockpitViewStore((s) => s.depthStyle);
  const layers = useMemo(() => effectiveLayers(level, flags, view), [level, flags, view]);
  const layerKey = useMemo(() => [...layers].sort().join(","), [layers]);

  // Only the fields the layout uses, so a 10 Hz pilot frame with an unchanged
  // mission does not re-render this component.
  const missionState = useNavStreamStore((s) => s.pilot?.data.mission?.state ?? null);
  const avoid = useNavStreamStore((s) => s.pilot?.data.avoidState ?? null);
  const showLoaded = useNavStreamStore((s) => {
    const p = s.pilot?.data;
    return !p || !p.armed || p.owner === "pilot" || p.owner === "fc_mode";
  });

  const visible = usePageVisible();
  const lost = isVideoLost(videoState, isDemoMode());
  const live = visible && !lost && view !== "map";
  const fpsCap = widthClass === "tv" ? KIOSK_FPS : 0;

  // Engine lifetime.
  if (engineRef.current === null) {
    engineRef.current = new LayerEngine(layerFrames, {
      cameraId,
      layers,
      level,
      videoW: videoWidth ?? 0,
      videoH: videoHeight ?? 0,
      mission: null,
      avoid: null,
      showLoaded: true,
      depthRangeM,
      depthMode,
      fpsCap,
    });
  }
  useEffect(() => {
    const engine = engineRef.current!;
    if (canvasRef.current) engine.attach(canvasRef.current);
    const box = boxRef.current;
    if (!box) return;
    const ro = new ResizeObserver(() => {
      const r = box.getBoundingClientRect();
      engine.resize(r.width, r.height, window.devicePixelRatio || 1);
    });
    ro.observe(box);
    // Exposed for the overlay budget checks (paint p95 per frame).
    window.__adosLayerStats = () => engine.stats();
    return () => {
      ro.disconnect();
      engine.stop();
    };
  }, []);

  // Configuration (camera, layers, level, mission, size, cap).
  useEffect(() => {
    const mission = missionState ? { state: missionState, seq: null, total: null, itemReached: null, id: null, source: null, ageMs: null, landingSeq: false, uploadsSeen: null } : null;
    engineRef.current!.configure({
      cameraId,
      layers,
      level,
      videoW: videoWidth ?? 0,
      videoH: videoHeight ?? 0,
      mission,
      avoid,
      showLoaded,
      depthRangeM,
      depthMode,
      fpsCap,
    });
    // `layerKey` stands for `layers` (a fresh Set each render).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cameraId, layerKey, level, videoWidth, videoHeight, missionState, avoid, showLoaded, depthRangeM, depthMode, fpsCap]);

  // Loop runs only while something is watched.
  useEffect(() => {
    const engine = engineRef.current!;
    if (live) engine.start();
    else engine.stop();
  }, [live]);

  // Nav-stream view lease for the on-screen camera.
  useEffect(() => {
    if (!live) {
      setViewLease(null);
      return;
    }
    const leased = LEASED_LAYERS.filter((id) => layers.has(id));
    setViewLease(leased.length > 0 ? { cameraId, layers: leased } : null);
    return () => setViewLease(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [live, cameraId, layerKey]);

  useVisionDemand(DEMAND_LAYERS[0], cameraId, live && layers.has("depth"));
  useVisionDemand(DEMAND_LAYERS[1], cameraId, live && layers.has("surfaces"));

  // `O` cycles the layer level (Full → Route → Off).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "o" && e.key !== "O") return;
      if (e.ctrlKey || e.metaKey || e.altKey || e.shiftKey) return;
      const t = e.target as HTMLElement | null;
      if (t && (t.isContentEditable || t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.tagName === "SELECT")) return;
      cycleLevel();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cycleLevel]);

  return (
    <div ref={boxRef} className="pointer-events-none absolute inset-0 z-[13]" data-testid="layer-stack" aria-hidden>
      <canvas ref={canvasRef} className="absolute inset-0 h-full w-full" />
    </div>
  );
}
