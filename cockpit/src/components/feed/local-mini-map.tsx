// The minimap's local frame (13-hud-v2 minimap, GPS-denied): north-up east /
// north metres around the drone. Layers bottom to top: grid, live occupancy
// tiles (`/api/v1/map/live`, map_server greys recoloured in the HUD ink),
// the SLAM trail from pose frames (dashed while degraded, broken where lost),
// the scan in the map frame, the origin, the follow target (dashed while
// coasting or searching, with a 1 s velocity tick) and the drone from the
// FC's LOCAL_POSITION_NED. Drag pans (the view recentres on the drone 10 s
// later), the wheel and the +/− buttons zoom.

import { useEffect, useRef, useState } from "react";
import { Minus, Plus } from "lucide-react";

import { useElementSize } from "@/hooks/use-element-size";
import { useNow } from "@/hooks/use-now";
import { useOccupancyImages } from "@/hooks/use-occupancy-images";
import { localDrone, localSceneInput, localTarget, panBy } from "@/lib/local-minimap";
import { cn } from "@/lib/utils";
import { buildLocalScene, DEFAULT_LOCAL_ZOOM, stepZoom, viewCentre, wheelZoom } from "@/shared/hud/local-scene";
import { useFlightStore } from "@/stores/flight-store";
import { ageOf, useNavStreamStore } from "@/stores/nav-stream-store";
import { useSettingsStore } from "@/stores/settings-store";

export function LocalMiniMap({ className }: { className?: string }) {
  const ref = useRef<HTMLDivElement | null>(null);
  const inkRef = useRef<HTMLSpanElement | null>(null);
  const { w, h } = useElementSize(ref);
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const pose = useNavStreamStore((s) => s.pose);
  const scan = useNavStreamStore((s) => s.scan);
  const follow = useNavStreamStore((s) => s.follow);
  const occupancy = useNavStreamStore((s) => s.occupancy);
  const occupancyAt = useNavStreamStore((s) => s.occupancyAt);
  const trail = useNavStreamStore((s) => s.trail);
  const now = useNow(250);
  const [pxPerM, setPxPerM] = useState(DEFAULT_LOCAL_ZOOM);
  const [pan, setPan] = useState<{ north: number; east: number; at: number } | null>(null);
  const drag = useRef<{ x: number; y: number; centre: { north: number; east: number } } | null>(null);
  const [inkCss, setInkCss] = useState<string | null>(null);
  const theme = useSettingsStore((s) => s.theme);

  // The HUD ink as the browser resolved it for the current theme, for
  // recolouring occupancy tiles.
  useEffect(() => {
    if (inkRef.current) setInkCss(getComputedStyle(inkRef.current).color);
  }, [theme]);
  const tileUrls = useOccupancyImages(occupancy, inkCss);

  const poseIn = pose ? { data: pose.data, ageMs: ageOf(pose, now) } : null;
  const drone = localDrone(telemetry, live, poseIn);
  const scene =
    w > 0 && h > 0
      ? buildLocalScene(
          localSceneInput({
            width: w,
            height: h,
            pxPerM,
            pan,
            now,
            drone,
            target: localTarget(follow ? { data: follow.data, ageMs: ageOf(follow, now) } : null),
            pose: poseIn,
            scan: scan ? { data: scan.data, ageMs: ageOf(scan, now) } : null,
            occupancy,
            occupancyAt,
            trail,
          }),
        )
      : null;

  // Wheel zoom needs a non-passive listener to keep the page from scrolling.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const onWheel = (e: WheelEvent) => {
      e.preventDefault();
      setPxPerM((z) => wheelZoom(z, e.deltaY));
    };
    el.addEventListener("wheel", onWheel, { passive: false });
    return () => el.removeEventListener("wheel", onWheel);
  }, []);

  return (
    <div
      ref={ref}
      className={cn("pointer-events-auto relative h-full w-full touch-none overflow-hidden", className)}
      onPointerDown={(e) => {
        e.currentTarget.setPointerCapture?.(e.pointerId);
        drag.current = { x: e.clientX, y: e.clientY, centre: viewCentre(pan, drone, now) };
      }}
      onPointerMove={(e) => {
        const d = drag.current;
        if (!d) return;
        // The event's epoch time: the pan ages against `useNow`'s clock.
        const at = performance.timeOrigin + e.timeStamp;
        setPan(panBy(d.centre, e.clientX - d.x, e.clientY - d.y, pxPerM, at));
      }}
      onPointerUp={() => {
        drag.current = null;
      }}
      onPointerCancel={() => {
        drag.current = null;
      }}
    >
      <span ref={inkRef} className="hidden text-hud-ink" aria-hidden />
      {scene ? (
        <svg width={w} height={h} className="absolute inset-0" aria-hidden>
          <g stroke="currentColor" className="text-surface-foreground/10">
            {scene.grid.map((l, i) => (
              <line key={i} x1={l.x1} y1={l.y1} x2={l.x2} y2={l.y2} />
            ))}
          </g>
          <g opacity={scene.dim ? 0.45 : 1}>
            {scene.tiles.map((t) =>
              tileUrls[t.key] ? (
                <image
                  key={t.key}
                  href={tileUrls[t.key]}
                  x={t.sx}
                  y={t.sy}
                  width={t.sizePx}
                  height={t.sizePx}
                  preserveAspectRatio="none"
                  style={{ imageRendering: "pixelated" }}
                />
              ) : null,
            )}
          </g>
          <g fill="none" stroke="currentColor" className="text-hud-primary" strokeWidth={1.6} strokeLinejoin="round">
            {scene.trail.map((run, i) => (
              <polyline
                key={i}
                points={run.points.map((p) => `${p[0].toFixed(1)},${p[1].toFixed(1)}`).join(" ")}
                strokeDasharray={run.dashed ? "4 3" : undefined}
                opacity={0.7}
              />
            ))}
          </g>
          <g className="text-hud-ink" fill="currentColor" opacity={0.85}>
            {scene.scan.map((p, i) => (
              <circle key={i} cx={p[0]} cy={p[1]} r={1.1} />
            ))}
          </g>
          <g className="text-muted-foreground" stroke="currentColor" strokeWidth={1}>
            <line x1={scene.origin.sx - 4} y1={scene.origin.sy} x2={scene.origin.sx + 4} y2={scene.origin.sy} />
            <line x1={scene.origin.sx} y1={scene.origin.sy - 4} x2={scene.origin.sx} y2={scene.origin.sy + 4} />
          </g>
          {scene.target ? (
            <g
              className={scene.target.dashed ? "text-hud-coast" : "text-hud-track"}
              stroke="currentColor"
              fill="none"
              strokeWidth={2}
            >
              {scene.target.tickX !== null && scene.target.tickY !== null ? (
                <line x1={scene.target.sx} y1={scene.target.sy} x2={scene.target.tickX} y2={scene.target.tickY} />
              ) : null}
              <circle
                cx={scene.target.sx}
                cy={scene.target.sy}
                r={5}
                strokeDasharray={scene.target.dashed ? "3 2" : undefined}
              />
            </g>
          ) : null}
          {scene.drone ? (
            <g className="text-hud-primary" fill="currentColor" stroke="rgba(0,0,0,0.65)" strokeWidth={1}>
              <polygon
                points="0,-7 5,6 0,3 -5,6"
                transform={`translate(${scene.drone.sx} ${scene.drone.sy}) rotate(${scene.drone.yawDeg})`}
              />
            </g>
          ) : null}
        </svg>
      ) : null}

      {!scene?.drone ? (
        <div className="absolute inset-0 flex items-center justify-center px-[0.3rem] text-center text-[0.75rem] text-muted-foreground">
          No local position
        </div>
      ) : null}

      <div className="absolute bottom-[0.1rem] left-[0.3rem] flex items-end gap-[0.3rem] font-mono text-[0.7rem] text-muted-foreground">
        {scene ? (
          <span className="flex flex-col items-start">
            <span className="block h-[2px] bg-current" style={{ width: scene.scaleBar.px }} aria-hidden />
            {scene.scaleBar.m} m
          </span>
        ) : null}
        {scene?.dim && Object.keys(occupancy.tiles).length > 0 ? <span className="text-hud-warn">MAP STALE</span> : null}
      </div>
      <div className="absolute bottom-[0.15rem] right-[0.15rem] flex gap-[0.2rem]">
        <button
          type="button"
          aria-label="Zoom out"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={() => setPxPerM((z) => stepZoom(z, -1))}
          className="flex h-[28px] w-[28px] items-center justify-center rounded bg-hud-glass text-hud-ink"
        >
          <Minus className="h-[0.8rem] w-[0.8rem]" aria-hidden />
        </button>
        <button
          type="button"
          aria-label="Zoom in"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={() => setPxPerM((z) => stepZoom(z, 1))}
          className="flex h-[28px] w-[28px] items-center justify-center rounded bg-hud-glass text-hud-ink"
        >
          <Plus className="h-[0.8rem] w-[0.8rem]" aria-hidden />
        </button>
      </div>
    </div>
  );
}
