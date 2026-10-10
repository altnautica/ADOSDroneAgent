// L2 — the instrument HUD, drawn on one canvas over the video. It reads the
// flight store once per animation frame (and repaints only when the sample,
// the staleness or the size changed), so telemetry never re-renders React.
//
// Instruments: horizon and pitch ladder (rungs every 5°, labels every 10°),
// roll arc with ±10/20/30/45/60° ticks, flight-path marker from the velocity
// vector, heading tape with a home-bearing caret, scrolling speed tape
// (groundspeed; airspeed on a fixed-wing when it reads > 0), scrolling
// altitude tape (REL/MSL, persisted) with a vertical-speed bar, and a centre
// crosshair. The agent's telemetry carries no wind, so no wind arrow is
// drawn. Anything whose source is older than 2 s dims and reads a dash.

import { useEffect, useRef } from "react";

import {
  ROLL_TICKS,
  angleDelta,
  flightPath,
  headingTicks,
  isInstrumentStale,
  pitchRungs,
  tapeTicks,
} from "@/lib/hud-geometry";
import type { VehicleState } from "@/lib/types";
import { bearingDeg, haversineM } from "@/shared/format";
import { useFlightStore, type HomePoint } from "@/stores/flight-store";
import { useSettingsStore, type AltRef } from "@/stores/settings-store";

const RAD = 180 / Math.PI;
const MAV_TYPE_FIXED_WING = 1;
const DASH = "—";

interface Palette {
  primary: string;
  ink: string;
  ink2: string;
  glass: string;
}

function finite(v: number | null | undefined): v is number {
  return typeof v === "number" && Number.isFinite(v);
}

function draw(
  ctx: CanvasRenderingContext2D,
  w: number,
  h: number,
  u: number,
  p: Palette,
  t: VehicleState | null,
  stale: boolean,
  home: HomePoint | null,
  altRef: AltRef,
) {
  ctx.clearRect(0, 0, w, h);
  const cx = w / 2;
  const cy = h / 2;
  const R = Math.max(40, Math.min(w * 0.26, h * 0.4));
  const k = R / 25;
  const font = (scale: number) => `${Math.round(scale * u)}px "JetBrains Mono", ui-monospace, monospace`;
  ctx.lineCap = "round";
  ctx.shadowColor = "rgba(0,0,0,0.85)";
  ctx.shadowBlur = 2;
  ctx.globalAlpha = stale ? 0.4 : 1;

  const roll = !stale && finite(t?.attitude?.roll) ? t!.attitude!.roll! * RAD : null;
  const pitch = !stale && finite(t?.attitude?.pitch) ? t!.attitude!.pitch! * RAD : null;
  const heading = !stale && finite(t?.position?.heading) && t!.position!.heading! <= 360 ? t!.position!.heading! : null;

  // Horizon + ladder, clipped to the attitude window.
  if (roll !== null && pitch !== null) {
    ctx.save();
    ctx.beginPath();
    ctx.rect(cx - R * 1.2, cy - R, R * 2.4, R * 2);
    ctx.clip();
    ctx.translate(cx, cy);
    ctx.rotate((-roll * Math.PI) / 180);
    ctx.strokeStyle = p.primary;
    ctx.fillStyle = p.primary;
    ctx.lineWidth = Math.max(1.5, u * 0.08);
    const hy = pitch * k;
    ctx.beginPath();
    ctx.moveTo(-R * 3, hy);
    ctx.lineTo(-R * 0.18, hy);
    ctx.moveTo(R * 0.18, hy);
    ctx.lineTo(R * 3, hy);
    ctx.stroke();
    ctx.lineWidth = Math.max(1, u * 0.05);
    ctx.font = font(0.6);
    ctx.textBaseline = "middle";
    for (const rung of pitchRungs(pitch, k, 22)) {
      const half = rung.label ? R * 0.3 : R * 0.16;
      ctx.setLineDash(rung.deg < 0 ? [u * 0.3, u * 0.2] : []);
      ctx.beginPath();
      ctx.moveTo(-half, rung.y);
      ctx.lineTo(half, rung.y);
      ctx.stroke();
      if (rung.label) {
        const txt = String(Math.abs(rung.deg));
        ctx.textAlign = "right";
        ctx.fillText(txt, -half - u * 0.2, rung.y);
        ctx.textAlign = "left";
        ctx.fillText(txt, half + u * 0.2, rung.y);
      }
    }
    ctx.setLineDash([]);
    ctx.restore();
  } else {
    ctx.strokeStyle = p.ink2;
    ctx.setLineDash([u * 0.3, u * 0.3]);
    ctx.beginPath();
    ctx.moveTo(cx - R * 0.6, cy);
    ctx.lineTo(cx - R * 0.18, cy);
    ctx.moveTo(cx + R * 0.18, cy);
    ctx.lineTo(cx + R * 0.6, cy);
    ctx.stroke();
    ctx.setLineDash([]);
    ctx.fillStyle = p.ink2;
    ctx.font = font(0.7);
    ctx.textAlign = "center";
    ctx.fillText("NO ATTITUDE", cx, cy - u * 0.9);
  }

  // Roll arc + pointer.
  const ra = R * 1.02;
  ctx.strokeStyle = p.ink;
  ctx.lineWidth = Math.max(1, u * 0.05);
  for (const a of [0, ...ROLL_TICKS]) {
    const len = a === 0 ? 0 : Math.abs(a) === 30 || Math.abs(a) === 60 ? u * 0.55 : u * 0.32;
    const s = Math.sin((a * Math.PI) / 180);
    const c = Math.cos((a * Math.PI) / 180);
    if (len > 0) {
      ctx.beginPath();
      ctx.moveTo(cx + ra * s, cy - ra * c);
      ctx.lineTo(cx + (ra + len) * s, cy - (ra + len) * c);
      ctx.stroke();
    }
  }
  ctx.fillStyle = p.ink;
  ctx.beginPath();
  ctx.moveTo(cx, cy - ra - u * 0.05);
  ctx.lineTo(cx - u * 0.25, cy - ra - u * 0.45);
  ctx.lineTo(cx + u * 0.25, cy - ra - u * 0.45);
  ctx.fill();
  if (roll !== null) {
    ctx.save();
    ctx.translate(cx, cy);
    ctx.rotate((-roll * Math.PI) / 180);
    ctx.fillStyle = p.primary;
    ctx.beginPath();
    ctx.moveTo(0, -ra + u * 0.1);
    ctx.lineTo(-u * 0.25, -ra + u * 0.5);
    ctx.lineTo(u * 0.25, -ra + u * 0.5);
    ctx.fill();
    ctx.restore();
  }

  // Centre crosshair.
  ctx.strokeStyle = p.ink;
  ctx.lineWidth = Math.max(1.5, u * 0.08);
  ctx.beginPath();
  ctx.moveTo(cx - u * 1.6, cy);
  ctx.lineTo(cx - u * 0.6, cy);
  ctx.lineTo(cx - u * 0.6, cy + u * 0.4);
  ctx.moveTo(cx + u * 1.6, cy);
  ctx.lineTo(cx + u * 0.6, cy);
  ctx.lineTo(cx + u * 0.6, cy + u * 0.4);
  ctx.stroke();
  ctx.beginPath();
  ctx.arc(cx, cy, u * 0.12, 0, Math.PI * 2);
  ctx.stroke();

  // Flight-path marker.
  const fp = stale ? null : flightPath(t?.velocity?.vx, t?.velocity?.vy, t?.velocity?.vz, heading);
  if (fp && pitch !== null) {
    const x = cx + Math.max(-R, Math.min(R, fp.driftDeg * k));
    const y = cy - Math.max(-R, Math.min(R, (fp.fpaDeg - pitch) * k));
    ctx.strokeStyle = p.primary;
    ctx.lineWidth = Math.max(1.5, u * 0.07);
    ctx.beginPath();
    ctx.arc(x, y, u * 0.35, 0, Math.PI * 2);
    ctx.moveTo(x - u * 0.35, y);
    ctx.lineTo(x - u * 0.9, y);
    ctx.moveTo(x + u * 0.35, y);
    ctx.lineTo(x + u * 0.9, y);
    ctx.moveTo(x, y - u * 0.35);
    ctx.lineTo(x, y - u * 0.7);
    ctx.stroke();
  }

  // Heading tape (top).
  const tapeW = Math.min(w * 0.5, R * 2.4);
  const tx0 = cx - tapeW / 2;
  const ty = u * 0.3;
  const degPx = tapeW / 60;
  ctx.font = font(0.6);
  ctx.textAlign = "center";
  ctx.textBaseline = "top";
  ctx.strokeStyle = p.ink;
  ctx.fillStyle = p.ink;
  ctx.lineWidth = Math.max(1, u * 0.05);
  if (heading !== null) {
    for (const tick of headingTicks(heading, 30, 5)) {
      const x = cx + tick.delta * degPx;
      const major = tick.deg % 10 === 0;
      ctx.beginPath();
      ctx.moveTo(x, ty);
      ctx.lineTo(x, ty + (major ? u * 0.45 : u * 0.25));
      ctx.stroke();
      if (major && Math.abs(tick.delta) > 4) {
        const card: Record<number, string> = { 0: "N", 90: "E", 180: "S", 270: "W" };
        ctx.fillText(card[tick.deg] ?? String(tick.deg / 10).padStart(2, "0"), x, ty + u * 0.5);
      }
    }
    // Home-bearing caret, pinned to the tape edge when home is off-tape.
    const lat = t?.position?.lat;
    const lon = t?.position?.lon;
    if (home && finite(lat) && finite(lon)) {
      const d = angleDelta(bearingDeg(lat, lon, home.lat, home.lon), heading);
      const x = cx + Math.max(-30, Math.min(30, d)) * degPx;
      ctx.fillStyle = p.primary;
      ctx.beginPath();
      ctx.moveTo(x, ty + u * 1.15);
      ctx.lineTo(x - u * 0.3, ty + u * 1.6);
      ctx.lineTo(x + u * 0.3, ty + u * 1.6);
      ctx.fill();
      ctx.font = font(0.6);
      ctx.fillText(`H ${Math.round(haversineM(lat, lon, home.lat, home.lon))} m`, x, ty + u * 1.7);
    }
  }
  ctx.fillStyle = p.glass;
  ctx.fillRect(cx - u * 1.4, ty + u * 0.45, u * 2.8, u * 0.95);
  ctx.strokeStyle = p.primary;
  ctx.strokeRect(cx - u * 1.4, ty + u * 0.45, u * 2.8, u * 0.95);
  ctx.fillStyle = p.ink;
  ctx.font = font(0.75);
  ctx.textBaseline = "middle";
  ctx.fillText(heading !== null ? `${String(Math.round(heading) % 360).padStart(3, "0")}°` : DASH, cx, ty + u * 0.93);
  void tx0;

  // Speed tape (left) and altitude tape (right).
  const fixedWing = t?.mav_type === MAV_TYPE_FIXED_WING;
  const air = t?.velocity?.airspeed;
  const useAir = fixedWing && finite(air) && air > 0;
  const speed = stale ? null : useAir ? (air as number) : (t?.velocity?.groundspeed ?? null);
  const altRaw = altRef === "msl" ? t?.position?.alt_msl : t?.position?.alt_rel;
  const alt = stale || !finite(altRaw) ? null : altRaw;
  const tapeH = R * 1.8;
  const tapeTop = cy - tapeH / 2;
  const tape = (x: number, value: number | null, halfSpan: number, step: number, labelEvery: number, side: "l" | "r", title: string) => {
    const pxPer = tapeH / (halfSpan * 2);
    ctx.strokeStyle = p.ink;
    ctx.fillStyle = p.ink;
    ctx.lineWidth = Math.max(1, u * 0.05);
    ctx.beginPath();
    ctx.moveTo(x, tapeTop);
    ctx.lineTo(x, tapeTop + tapeH);
    ctx.stroke();
    ctx.font = font(0.6);
    ctx.textBaseline = "middle";
    ctx.textAlign = side === "l" ? "right" : "left";
    ctx.fillText(title, x, tapeTop - u * 0.5);
    if (value !== null) {
      for (const v of tapeTicks(value, halfSpan, step)) {
        const y = cy + (value - v) * pxPer;
        const major = Math.abs(v % labelEvery) < 1e-6;
        const len = major ? u * 0.4 : u * 0.22;
        ctx.beginPath();
        ctx.moveTo(x, y);
        ctx.lineTo(side === "l" ? x - len : x + len, y);
        ctx.stroke();
        if (major) ctx.fillText(String(v), side === "l" ? x - u * 0.55 : x + u * 0.55, y);
      }
    }
    const bw = u * 2.6;
    const bx = side === "l" ? x - bw - u * 0.1 : x + u * 0.1;
    ctx.fillStyle = p.glass;
    ctx.fillRect(bx, cy - u * 0.5, bw, u);
    ctx.strokeStyle = p.primary;
    ctx.strokeRect(bx, cy - u * 0.5, bw, u);
    ctx.fillStyle = p.ink;
    ctx.font = font(0.8);
    ctx.textAlign = "center";
    ctx.fillText(value !== null ? value.toFixed(value < 100 && value > -100 ? 1 : 0) : DASH, bx + bw / 2, cy);
  };
  tape(u * 3.4, speed, 10, 1, 5, "l", useAir ? "AS m/s" : "GS m/s");
  const altX = w - u * 3.4;
  tape(altX, alt, 30, 5, 10, "r", altRef === "msl" ? "ALT MSL" : "ALT REL");

  // Vertical-speed bar beside the altitude tape (±5 m/s full scale).
  const climb = stale ? null : (t?.velocity?.climb ?? null);
  const vsX = altX - u * 0.6;
  ctx.strokeStyle = p.ink2;
  ctx.beginPath();
  ctx.moveTo(vsX, cy - tapeH / 2);
  ctx.lineTo(vsX, cy + tapeH / 2);
  ctx.stroke();
  if (finite(climb)) {
    const len = (Math.max(-5, Math.min(5, climb)) / 5) * (tapeH / 2);
    ctx.strokeStyle = p.primary;
    ctx.lineWidth = Math.max(2, u * 0.15);
    ctx.beginPath();
    ctx.moveTo(vsX, cy);
    ctx.lineTo(vsX, cy - len);
    ctx.stroke();
  }
  ctx.fillStyle = p.ink;
  ctx.font = font(0.6);
  ctx.textAlign = "right";
  ctx.fillText(`V/S ${finite(climb) ? `${climb >= 0 ? "+" : ""}${climb.toFixed(1)}` : DASH}`, altX - u * 0.2, tapeTop + tapeH + u * 0.6);
  ctx.globalAlpha = 1;
}

export function HudCanvas() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const altRef = useSettingsStore((s) => s.altRef);
  const setAltRef = useSettingsStore((s) => s.setAltRef);
  const altRefRef = useRef(altRef);

  useEffect(() => {
    altRefRef.current = altRef;
  }, [altRef]);

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    const css = getComputedStyle(document.documentElement);
    const v = (name: string, fallback: string) => css.getPropertyValue(name).trim() || fallback;
    const palette: Palette = {
      primary: v("--hud-primary", "#5B9AFF"),
      ink: v("--hud-ink", "#F8FAFC"),
      ink2: v("--hud-ink-2", "#94A3B8"),
      glass: v("--hud-glass-strong", "rgba(9,15,28,0.78)"),
    };
    let w = 0;
    let h = 0;
    let u = 16;
    let sized = 0;
    const resize = () => {
      const rect = canvas.getBoundingClientRect();
      const dpr = window.devicePixelRatio || 1;
      w = rect.width;
      h = rect.height;
      u = parseFloat(getComputedStyle(document.documentElement).fontSize) || 16;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      sized += 1;
    };
    const ro = new ResizeObserver(resize);
    ro.observe(canvas);
    resize();

    let raf = 0;
    let painted: unknown[] = [];
    const frame = () => {
      raf = requestAnimationFrame(frame);
      const s = useFlightStore.getState();
      const stale = isInstrumentStale(performance.now(), s.lastLiveAt, s.live);
      const key = [s.telemetry, stale, s.home, altRefRef.current, sized];
      if (key.every((x, i) => x === painted[i])) return;
      painted = key;
      draw(ctx, w, h, u, palette, s.telemetry, stale, s.home, altRefRef.current);
    };
    raf = requestAnimationFrame(frame);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
    };
  }, []);

  return (
    <div className="pointer-events-none absolute inset-0 z-[15]">
      <canvas ref={canvasRef} className="h-full w-full" aria-hidden />
      <button
        type="button"
        onClick={() => setAltRef(altRef === "rel" ? "msl" : "rel")}
        aria-label={`Altitude reference ${altRef === "rel" ? "home-relative" : "mean sea level"}; switch`}
        className="pointer-events-auto absolute right-0 top-1/2 min-h-[40px] -translate-y-[calc(50%+6rem)] rounded-md border border-hud-hair bg-hud-glass px-[0.4rem] font-mono text-[0.75rem] text-hud-ink"
      >
        {altRef === "rel" ? "REL" : "MSL"}
      </button>
    </div>
  );
}
