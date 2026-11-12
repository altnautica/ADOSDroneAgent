// The HUD's centre marks, drawn at every layer level: the reticle, a short
// horizon mark either side of it that follows roll and pitch, and a small
// heading readout above it with a dark halo under its glyphs.

import tokensJson from "@/shared/cockpit-spec/tokens.json";

export interface Palette {
  primary: string;
  ink: string;
  ink2: string;
  glass: string;
}

/** The heading readout's halo (`tokens.json` `ar.halo`), 3 px wide. */
export const HEADING_HALO: string = tokensJson.ar.halo;
export const HEADING_HALO_PX = 3;

/** The centre reticle: two open brackets and a centre dot. */
export function drawReticle(ctx: CanvasRenderingContext2D, cx: number, cy: number, u: number, p: Palette) {
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
}

/** The clean centre: the reticle, a short horizon mark either side of it
 *  that follows roll and pitch (clamped near the centre), and the heading as
 *  a small readout above the reticle. Nothing else crosses the picture. */
export function drawCentreMarks(
  ctx: CanvasRenderingContext2D,
  cx: number,
  cy: number,
  u: number,
  p: Palette,
  roll: number | null,
  pitch: number | null,
  heading: number | null,
  font: (scale: number) => string,
) {
  drawReticle(ctx, cx, cy, u, p);
  if (roll !== null && pitch !== null) {
    const off = Math.max(-u * 4, Math.min(u * 4, pitch * u * 0.25));
    ctx.save();
    ctx.translate(cx, cy + off);
    ctx.rotate((-roll * Math.PI) / 180);
    ctx.strokeStyle = p.ink;
    ctx.lineWidth = Math.max(1.5, u * 0.08);
    ctx.beginPath();
    ctx.moveTo(-u * 4.2, 0);
    ctx.lineTo(-u * 2.4, 0);
    ctx.moveTo(u * 2.4, 0);
    ctx.lineTo(u * 4.2, 0);
    ctx.stroke();
    ctx.restore();
  }
  ctx.fillStyle = p.ink;
  ctx.font = font(0.75);
  ctx.textAlign = "center";
  ctx.textBaseline = "middle";
  const text = heading !== null ? `${String(Math.round(heading) % 360).padStart(3, "0")}°` : "—";
  // A dark halo under the glyphs, so the readout holds over a bright picture.
  ctx.save();
  ctx.strokeStyle = HEADING_HALO;
  ctx.lineWidth = HEADING_HALO_PX;
  ctx.lineJoin = "round";
  ctx.strokeText(text, cx, cy - u * 1.6);
  ctx.restore();
  ctx.fillText(text, cx, cy - u * 1.6);
}
