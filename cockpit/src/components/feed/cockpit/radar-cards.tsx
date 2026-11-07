// The bottom-left card's instrument faces: the obstacle radar (the pilot's 72
// clearance sectors, 5° each clockwise from the nose, from the `obstacles`
// frame) and the attitude card (roll/pitch horizon from telemetry beside the
// same radar). The obstacle frame lives in the layer stack's mutable store,
// not React state: the canvas polls it at 10 Hz and repaints only when its
// version changes, and hides the sectors once the frame is 2 s old.

import { useEffect, useRef, useState } from "react";

import { RADAR_RANGE_M, RADAR_STALE_MS, sectorRadius } from "@/lib/cockpit-flow";
import { layerFrames } from "@/lib/layer-frames";
import { cn } from "@/lib/utils";
import { STATE_COLORS } from "@/lib/cockpit-spec";
import { nearestSector } from "@/shared/hud/layer-layout";
import { useFlightStore } from "@/stores/flight-store";

const POLL_MS = 100;

function sectorColor(d: number): string {
  if (d <= 2) return STATE_COLORS.blocked ?? "#EF4444";
  if (d <= 5) return STATE_COLORS.avoiding ?? "#F59E0B";
  return "rgba(237,237,237,0.55)";
}

function paint(canvas: HTMLCanvasElement, sectors: readonly (number | null)[] | null): void {
  const dpr = window.devicePixelRatio || 1;
  const w = canvas.clientWidth;
  const h = canvas.clientHeight;
  if (canvas.width !== Math.round(w * dpr)) canvas.width = Math.round(w * dpr);
  if (canvas.height !== Math.round(h * dpr)) canvas.height = Math.round(h * dpr);
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, w, h);
  const cx = w / 2;
  const cy = h / 2;
  const r = Math.min(w, h) / 2 - 4;
  ctx.strokeStyle = "rgba(237,237,237,0.25)";
  ctx.lineWidth = 1;
  for (const f of [0.2, 0.5, 1]) {
    ctx.beginPath();
    ctx.arc(cx, cy, r * f, 0, Math.PI * 2);
    ctx.stroke();
  }
  // Nose marker.
  ctx.fillStyle = "rgba(237,237,237,0.8)";
  ctx.beginPath();
  ctx.moveTo(cx, cy - 6);
  ctx.lineTo(cx - 4, cy + 4);
  ctx.lineTo(cx + 4, cy + 4);
  ctx.fill();
  if (!sectors) return;
  const step = (Math.PI * 2) / sectors.length;
  for (let i = 0; i < sectors.length; i++) {
    const d = sectors[i];
    const f = sectorRadius(d);
    if (f === null || d === null || d > RADAR_RANGE_M) continue;
    // Body yaw 0 = nose = up, clockwise.
    const a0 = -Math.PI / 2 + i * step - step / 2;
    ctx.strokeStyle = sectorColor(d);
    ctx.lineWidth = 3;
    ctx.beginPath();
    ctx.arc(cx, cy, Math.max(2, r * f), a0, a0 + step);
    ctx.stroke();
  }
}

function useObstacleCanvas(): React.RefObject<HTMLCanvasElement> {
  const ref = useRef<HTMLCanvasElement>(null);
  useEffect(() => {
    let version = -1;
    let shown = false;
    const tick = () => {
      const c = ref.current;
      if (!c || document.visibilityState === "hidden") return;
      const slot = layerFrames.obstacles;
      const fresh = slot.data !== null && performance.now() - slot.at <= RADAR_STALE_MS;
      if (fresh && slot.version === version && shown) return;
      if (!fresh && !shown && version !== -1) return;
      version = slot.version;
      shown = fresh;
      paint(c, fresh ? (slot.data?.sectorsM ?? null) : null);
    };
    tick();
    const id = setInterval(tick, POLL_MS);
    return () => clearInterval(id);
  }, []);
  return ref;
}

export function ObstacleRadarCard({ className }: { className?: string }) {
  const ref = useObstacleCanvas();
  const nearest = useNearestLabel();
  return (
    <div className={cn("relative", className)} data-testid="obstacle-radar">
      <canvas ref={ref} className="h-full w-full" aria-label="Obstacle radar" />
      <span className="absolute bottom-[0.2rem] left-1/2 -translate-x-1/2 font-mono text-[0.68rem] text-hud-ink">{nearest}</span>
    </div>
  );
}

/** The nearest obstacle as a label, re-read at 2 Hz; re-renders only when
 *  the label changes. */
function useNearestLabel(): string {
  const [label, setLabel] = useState("—");
  useEffect(() => {
    const id = setInterval(() => {
      const slot = layerFrames.obstacles;
      const fresh = slot.data !== null && performance.now() - slot.at <= RADAR_STALE_MS;
      const n = fresh && slot.data ? nearestSector(slot.data.sectorsM) : null;
      setLabel(n === null ? "—" : `${n.toFixed(1)} m`);
    }, 500);
    return () => clearInterval(id);
  }, []);
  return label;
}

/** Attitude (roll/pitch horizon) beside the obstacle radar. */
export function AttitudeRadarCard({ className }: { className?: string }) {
  const roll = useFlightStore((s) => s.telemetry?.attitude?.roll ?? null);
  const pitch = useFlightStore((s) => s.telemetry?.attitude?.pitch ?? null);
  const live = useFlightStore((s) => s.live);
  const rollDeg = roll === null ? 0 : (roll * 180) / Math.PI;
  const pitchDeg = pitch === null ? 0 : (pitch * 180) / Math.PI;
  return (
    <div className={cn("flex items-stretch gap-[0.2rem] p-[0.2rem]", className)} data-testid="attitude-radar">
      <svg viewBox="-50 -50 100 100" className={cn("h-full flex-1", !live && "opacity-40")} aria-label="Attitude">
        <clipPath id="att-clip">
          <circle r="46" />
        </clipPath>
        <g clipPath="url(#att-clip)" transform={`rotate(${-rollDeg})`}>
          <rect x="-80" y={-80 + pitchDeg * 1.5} width="160" height="80" fill="#2979FF" opacity="0.45" />
          <rect x="-80" y={pitchDeg * 1.5} width="160" height="80" fill="#8D6E63" opacity="0.5" />
          <line x1="-80" x2="80" y1={pitchDeg * 1.5} y2={pitchDeg * 1.5} stroke="#EDEDED" strokeWidth="1.5" />
        </g>
        <circle r="46" fill="none" stroke="rgba(237,237,237,0.4)" />
        <path d="M-22 0 H-8 L0 6 L8 0 H22" fill="none" stroke="#FFD400" strokeWidth="2.5" />
      </svg>
      <ObstacleRadarCard className="h-full flex-1" />
    </div>
  );
}
