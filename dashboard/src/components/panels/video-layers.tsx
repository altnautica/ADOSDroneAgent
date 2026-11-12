// The dashboard's read-only data layers over its video: the cockpit's shared
// layer engine at the Route level (AR route and motion items, obstacles),
// plus the depth and surfaces layers when their toggle is on (off by
// default, as `layers.json` defaults them). Frames come from the shared
// nav-status poll (5 Hz while this panel is mounted). While the panel shows
// live video the poll carries the view lease `?view=main:<layers>`, so the
// agent computes these layers for the primary camera only while this panel
// watches; a toggled-on depth or surfaces layer joins the same lease and
// holds a vision demand lease of its kind, which is what makes the engine run
// it.
//
// The dashboard is the setup surface: the layer level and views live in the
// cockpit. The two leased toggles are here because they cost the agent work.

import { useQuery } from "@tanstack/react-query";
import { useEffect, useRef } from "react";

import { useNavLease, useNavStatus, useNavVideoPanel } from "@/hooks/use-nav-status";
import { holdLayerDemand, LEASED_LAYERS, layersViewQuery, type DemandKind } from "@/lib/video-layer-lease";
import { cn } from "@/lib/utils";
import { apiFetch } from "@/shared/api-fetch";
import {
  parseArFrame,
  parseDepthFrame,
  parseObstaclesFrame,
  parsePilotState,
  parseSurfacesFrame,
} from "@/shared/hud/cockpit-frames";
import { LayerEngine, LayerFrameStore } from "@/shared/hud/layer-engine";
import { specText } from "@/shared/hud/spec-text";

const LAYERS_LABEL = specText("cockpit.layers.flags");
const GIMBAL_TEXT = specText("cockpit.gimbal_pad");
const GIMBAL_OFF_TEXT = `${GIMBAL_TEXT} · ${specText("cockpit.unavailable")}`;

/** The engine's video size: the element's intrinsic size once known. */
function videoSize(v: HTMLVideoElement | null): { videoW: number; videoH: number } | null {
  return v && v.videoWidth > 0 && v.videoHeight > 0 ? { videoW: v.videoWidth, videoH: v.videoHeight } : null;
}

/** Hold a vision demand lease of `kind` while `active` (best effort: an
 *  unanswered POST still lapses after the hold). */
function useLayerDemand(kind: DemandKind, active: boolean, cameraId: string): void {
  useEffect(() => {
    if (!active) return;
    return holdLayerDemand(kind, (body) => void apiFetch("/api/vision/demand", { method: "POST", body }).catch(() => undefined), document, cameraId);
  }, [kind, active, cameraId]);
}

export function VideoLayers({
  active,
  videoRef,
  toggles,
  cameraId,
}: {
  /** True while the panel shows live video. */
  active: boolean;
  videoRef: React.RefObject<HTMLVideoElement | null>;
  /** Leased layer id → on. */
  toggles: Readonly<Record<string, boolean>>;
  /** The camera the layers are computed for. */
  cameraId: string;
}) {
  const boxRef = useRef<HTMLDivElement | null>(null);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  const engineRef = useRef<{ engine: LayerEngine; store: LayerFrameStore; mission: string } | null>(null);
  const view = layersViewQuery(toggles, cameraId);
  useNavVideoPanel();
  useNavLease(active ? view : null);
  useLayerDemand("depth", active && toggles.depth === true, cameraId);
  useLayerDemand("surfaces", active && toggles.surfaces === true, cameraId);
  const nav = useNavStatus();

  useEffect(() => {
    const box = boxRef.current;
    const canvas = canvasRef.current;
    if (!active || !box || !canvas) return;
    const store = new LayerFrameStore();
    const engine = new LayerEngine(store, {
      cameraId,
      layers: new Set(view.slice(cameraId.length + 1).split(",")),
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
    engineRef.current = { engine, store, mission: "" };
    // The letterbox follows the shown video: on a box resize, and when the
    // stream's metadata (or its size) arrives after the first layout.
    const fit = () => {
      const size = videoSize(videoRef.current);
      if (size) engine.configure(size);
    };
    const ro = new ResizeObserver(() => {
      const r = box.getBoundingClientRect();
      engine.resize(r.width, r.height, window.devicePixelRatio || 1);
      fit();
    });
    ro.observe(box);
    const video = videoRef.current;
    video?.addEventListener("loadedmetadata", fit);
    video?.addEventListener("resize", fit);
    fit();

    const onVis = () => (document.visibilityState === "hidden" ? engine.stop() : engine.start());
    document.addEventListener("visibilitychange", onVis);
    engine.start();
    return () => {
      document.removeEventListener("visibilitychange", onVis);
      video?.removeEventListener("loadedmetadata", fit);
      video?.removeEventListener("resize", fit);
      ro.disconnect();
      engine.stop();
      engineRef.current = null;
    };
  }, [active, videoRef, view, cameraId]);

  // Every poll goes to the store: a repeated `ts_ms` is the agent's
  // keep-alive, which refreshes only the receipt time (at - age_ms) so a
  // still scene's route stays up without re-interpolating.
  useEffect(() => {
    const cur = engineRef.current;
    const body = nav.data;
    if (!cur || !body || nav.isError) return;
    const { engine, store } = cur;
    const at = performance.now() - Math.max(0, Date.now() - nav.dataUpdatedAt);
    const pilot = body.pilot ? parsePilotState(body.pilot.data) : null;
    const missionKey = `${pilot?.mission?.state ?? ""}|${pilot?.avoidState ?? ""}|${pilot?.armed ?? ""}|${pilot?.owner ?? ""}`;
    if (missionKey !== cur.mission) {
      cur.mission = missionKey;
      engine.configure({
        mission: pilot?.mission ?? null,
        avoid: pilot?.avoidState ?? null,
        showLoaded: !pilot || !pilot.armed || pilot.owner === "pilot" || pilot.owner === "fc_mode",
      });
    }
    const age = (e: { age_ms?: number }) => at - (e.age_ms ?? 0);
    if (body.ar) {
      const f = parseArFrame(body.ar.data);
      if (f) store.putAr(f, age(body.ar));
    }
    if (body.obstacles) {
      const f = parseObstaclesFrame(body.obstacles.data);
      if (f) store.putObstacles(f, age(body.obstacles));
    }
    if (body.depth && toggles.depth) {
      const f = parseDepthFrame(body.depth.data);
      if (f) store.putDepth(f, age(body.depth));
    }
    if (body.surfaces && toggles.surfaces) {
      const f = parseSurfacesFrame(body.surfaces.data);
      if (f) store.putSurfaces(f, age(body.surfaces));
    }
  }, [nav.data, nav.dataUpdatedAt, nav.isError, toggles]);

  return (
    <div ref={boxRef} className="pointer-events-none absolute inset-0" aria-hidden>
      <canvas ref={canvasRef} className="absolute inset-0 h-full w-full" />
    </div>
  );
}

/** The depth and surfaces toggles (off by default). */
export function LayerToggles({
  toggles,
  onChange,
}: {
  toggles: Readonly<Record<string, boolean>>;
  onChange: (id: string, on: boolean) => void;
}) {
  return (
    <div role="group" aria-label={LAYERS_LABEL} className="flex items-center gap-3 text-xs">
      {LEASED_LAYERS.map((l) => (
        <label key={l.id} className="inline-flex cursor-pointer items-center gap-1.5 max-md:min-h-11">
          <input
            type="checkbox"
            className="h-3.5 w-3.5 accent-primary"
            checked={toggles[l.id] === true}
            onChange={(e) => onChange(l.id, e.target.checked)}
            data-layer={l.id}
          />
          {l.label}
        </label>
      ))}
    </div>
  );
}

/** The camera the detections and layers follow; hidden with only the primary. */
export function CameraSelect({
  options,
  value,
  onChange,
  label,
}: {
  options: readonly string[];
  value: string;
  onChange: (id: string) => void;
  label: string;
}) {
  if (options.length < 2) return null;
  return (
    <label className="inline-flex items-center gap-1.5 text-xs">
      <span className="text-muted-foreground">{label}</span>
      <select
        className="h-7 rounded-md border border-border bg-background px-2 text-xs"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        data-testid="camera-select"
      >
        {options.map((id) => (
          <option key={id} value={id}>
            {id}
          </option>
        ))}
      </select>
    </label>
  );
}

/** Whether the gimbal answers rate commands (`GET /api/v1/gimbal/rate`). */
export function GimbalBadge() {
  const q = useQuery<{ available?: boolean }>({
    queryKey: ["gimbal-rate"],
    queryFn: ({ signal }) => apiFetch<{ available?: boolean }>("/api/v1/gimbal/rate", { signal }),
    refetchInterval: 10_000,
    retry: false,
  });
  const available = !q.isError && q.data?.available === true;
  if (q.data === undefined && !q.isError) return null;
  return (
    <span
      className={cn(
        "rounded border px-1.5 py-0.5 text-[11px] uppercase tracking-wider",
        available ? "border-ok/40 text-ok" : "border-border text-muted-foreground",
      )}
      data-testid="gimbal-badge"
      data-available={available}
    >
      {available ? GIMBAL_TEXT : GIMBAL_OFF_TEXT}
    </span>
  );
}
