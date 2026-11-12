// L2: the HUD, drawn on one canvas over the video. It reads the flight store
// once per animation frame (and repaints only when the sample, the staleness,
// the size or the level changed), so telemetry never re-renders React.
//
// Two parts (cockpit spec `layers.json`):
// - centre marks, at every level: the centre reticle, a short horizon mark
//   that follows roll and pitch, and a small heading readout above the
//   reticle. The centre stays a clean camera view.
// - instruments, only at the Instruments level: horizon and pitch ladder
//   (rungs every 5°, labels every 10°), roll arc with ±10/20/30/45/60° ticks,
//   flight-path marker from the velocity vector, heading tape with a
//   home-bearing caret, scrolling speed tape (groundspeed; airspeed on a
//   fixed-wing when it reads > 0), scrolling altitude tape (REL/MSL,
//   persisted) with a vertical-speed bar.
// The agent's telemetry carries no wind, so no wind arrow is drawn. Anything
// whose source is older than 2 s dims and reads a dash.

import { useEffect, useRef } from "react";

import { useElementSize } from "@/hooks/use-element-size";
import { useMapChrome } from "@/hooks/use-map-chrome";
import { CHROME_TOKENS } from "@/lib/chrome-tokens";
import { drawCentreMarks, drawReticle, type Palette } from "@/components/feed/hud-centre-marks";
import { ALT_REF_BUTTON_H, HUD_UNIT_PX, altRefButtonPos, hudTapes } from "@/lib/hud-chrome";
import { HIT_AREA } from "@/lib/flight-layout";
import { cn } from "@/lib/utils";

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
import { fmtDistance, fmtSigned1 } from "@/shared/hud/format";
import { specText } from "@/shared/hud/spec-text";
import { useFlightStore, type HomePoint } from "@/stores/flight-store";
import { useSettingsStore, type AltRef } from "@/stores/settings-store";

const RAD = 180 / Math.PI;
const MAV_TYPE_FIXED_WING = 1;
const DASH = "—";

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
  instruments: boolean,
) {
  ctx.clearRect(0, 0, w, h);
  const cx = w / 2;
  const cy = h / 2;
  const { R, tapeH, tapeTop, speedX, altX } = hudTapes(w, h, u);
  const k = R / 25;
  const font = (scale: number) => `${Math.round(scale * u)}px "JetBrains Mono", ui-monospace, monospace`;
  ctx.lineCap = "round";
  ctx.shadowColor = "rgba(0,0,0,0.85)";
  ctx.shadowBlur = 2;
  ctx.globalAlpha = stale ? 0.4 : 1;

  const roll = !stale && finite(t?.attitude?.roll) ? t!.attitude!.roll! * RAD : null;
  const pitch = !stale && finite(t?.attitude?.pitch) ? t!.attitude!.pitch! * RAD : null;
  const heading = !stale && finite(t?.position?.heading) && t!.position!.heading! <= 360 ? t!.position!.heading! : null;

  if (!instruments) {
    drawCentreMarks(ctx, cx, cy, u, p, roll, pitch, heading, font);
    ctx.globalAlpha = 1;
    return;
  }

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
    ctx.fillText(specText("hud.no_attitude"), cx, cy - u * 0.9);
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

  drawReticle(ctx, cx, cy, u, p);

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
      ctx.fillText(`H ${fmtDistance(haversineM(lat, lon, home.lat, home.lon))}`, x, ty + u * 1.7);
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
  tape(speedX, speed, 10, 1, 5, "l", specText(useAir ? "hud.tape.as" : "hud.tape.gs"));
  tape(altX, alt, 30, 5, 10, "r", specText(altRef === "msl" ? "hud.tape.alt_msl" : "hud.tape.alt_rel"));

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
  ctx.fillText(specText("hud.vs", { v: fmtSigned1(climb) }), altX - u * 0.2, tapeTop + tapeH + u * 0.6);
  ctx.globalAlpha = 1;
}

export function HudCanvas({ instruments }: { instruments: boolean }) {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const rootRef = useRef<HTMLDivElement>(null);
  const size = useElementSize(rootRef);
  const { scale } = useMapChrome(rootRef);
  // The instruments' unit: chrome px times the class scale, not the root font.
  const u = HUD_UNIT_PX * scale;
  const unitRef = useRef(u);
  const altRef = useSettingsStore((s) => s.altRef);
  const setAltRef = useSettingsStore((s) => s.setAltRef);
  const altRefRef = useRef(altRef);
  const instrumentsRef = useRef(instruments);

  useEffect(() => {
    altRefRef.current = altRef;
    instrumentsRef.current = instruments;
  }, [altRef, instruments]);

  useEffect(() => {
    unitRef.current = u;
    // A new unit repaints through the resize path.
    canvasRef.current?.dispatchEvent(new Event("hud-unit"));
  }, [u]);

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    const css = getComputedStyle(document.documentElement);
    const v = (name: string, fallback: string) => css.getPropertyValue(name).trim() || fallback;
    const palette: Palette = {
      primary: v("--hud-primary", "#5B9AFF"),
      ink: v("--hud-ink", "#EDEDED"),
      ink2: v("--hud-ink-2", "#B4B4B4"),
      glass: v("--hud-glass-strong", "rgba(0,0,0,0.78)"),
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
      u = unitRef.current;
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      sized += 1;
    };
    const ro = new ResizeObserver(resize);
    ro.observe(canvas);
    canvas.addEventListener("hud-unit", resize);
    resize();

    let raf = 0;
    let paintedTelemetry: unknown = undefined;
    let paintedStale = false;
    let paintedHome: unknown = undefined;
    let paintedAlt: AltRef | null = null;
    let paintedSized = -1;
    let paintedInstruments = false;
    const frame = () => {
      raf = requestAnimationFrame(frame);
      const s = useFlightStore.getState();
      const stale = isInstrumentStale(performance.now(), s.lastLiveAt, s.live);
      const alt = altRefRef.current;
      const inst = instrumentsRef.current;
      if (
        s.telemetry === paintedTelemetry &&
        stale === paintedStale &&
        s.home === paintedHome &&
        alt === paintedAlt &&
        sized === paintedSized &&
        inst === paintedInstruments
      ) {
        return;
      }
      paintedTelemetry = s.telemetry;
      paintedStale = stale;
      paintedHome = s.home;
      paintedAlt = alt;
      paintedSized = sized;
      paintedInstruments = inst;
      draw(ctx, w, h, u, palette, s.telemetry, stale, s.home, alt, inst);
    };
    raf = requestAnimationFrame(frame);
    return () => {
      cancelAnimationFrame(raf);
      ro.disconnect();
      canvas.removeEventListener("hud-unit", resize);
    };
  }, []);

  const pos = altRefButtonPos(size.w, size.h, u);
  return (
    <div ref={rootRef} className="pointer-events-none absolute inset-0 z-[15]" data-testid="hud" data-hud-instruments={instruments ? "on" : "off"}>
      <canvas ref={canvasRef} className="h-full w-full" aria-hidden />
      {instruments && size.w > 0 ? (
        <button
          type="button"
          onClick={() => setAltRef(altRef === "rel" ? "msl" : "rel")}
          aria-label={specText("hud.alt_ref.switch", { ref: specText(altRef === "rel" ? "hud.alt_ref.rel_long" : "hud.alt_ref.msl_long") })}
          className={cn(HIT_AREA, "chrome-panel pointer-events-auto absolute !rounded-[var(--fs-radius-sm)] font-mono")}
          style={{ left: pos.x, top: pos.y, height: ALT_REF_BUTTON_H, padding: `0 ${6 * scale}px`, fontSize: CHROME_TOKENS.type.label_px * scale }}
          data-testid="alt-ref-toggle"
        >
          {specText(altRef === "rel" ? "hud.alt_ref.rel" : "hud.alt_ref.msl")}
        </button>
      ) : null}
    </div>
  );
}
