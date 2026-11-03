// Decoded, recoloured live-occupancy tiles for the local minimap. Each tile
// arrives as a greyscale map_server PNG; it is decoded once per (tile, seq,
// ink), every pixel classified (≤ 100 occupied, ≥ 230 free, else unknown)
// and painted in the HUD ink at 90 % / 10 % / transparent, and kept as a
// data URL so drawing it is one nearest-neighbour <image>.

import { useEffect, useMemo, useState } from "react";

import type { OccupancyGrid } from "@/shared/hud/local-frame";
import {
  FREE_ALPHA,
  OCCUPIED_ALPHA,
  parseCssColor,
  recolourOccupancy,
  withAlpha,
  type Rgba,
} from "@/shared/hud/occupancy-bitmap";

/** Decoded tiles kept across renders; bounded like the grid itself. */
const CACHE_LIMIT = 600;
const cache = new Map<string, string>();

function cacheKey(key: string, seq: number, ink: Rgba): string {
  return `${key}:${seq}:${ink.join(",")}`;
}

async function decodeTile(pngB64: string, ink: Rgba): Promise<string | null> {
  const img = new Image();
  img.src = `data:image/png;base64,${pngB64}`;
  try {
    await img.decode();
  } catch {
    return null;
  }
  const canvas = document.createElement("canvas");
  canvas.width = img.naturalWidth;
  canvas.height = img.naturalHeight;
  const ctx = canvas.getContext("2d");
  if (!ctx || canvas.width === 0) return null;
  ctx.drawImage(img, 0, 0);
  const data = ctx.getImageData(0, 0, canvas.width, canvas.height);
  recolourOccupancy(data.data, withAlpha(ink, OCCUPIED_ALPHA), withAlpha(ink, FREE_ALPHA));
  ctx.putImageData(data, 0, 0);
  return canvas.toDataURL("image/png");
}

/** Tile key → recoloured data URL for every tile decoded so far. `inkCss` is
 *  the computed HUD ink colour; with none parsed, nothing is drawn. */
export function useOccupancyImages(grid: OccupancyGrid, inkCss: string | null): Record<string, string> {
  const [, setVersion] = useState(0);
  const ink = useMemo(() => (inkCss ? parseCssColor(inkCss) : null), [inkCss]);

  useEffect(() => {
    if (!ink) return;
    let cancelled = false;
    const missing = Object.entries(grid.tiles).filter(([key, t]) => !cache.has(cacheKey(key, t.seq, ink)));
    if (missing.length === 0) return;
    void Promise.all(
      missing.map(async ([key, t]) => {
        const url = await decodeTile(t.pngB64, ink);
        if (url) cache.set(cacheKey(key, t.seq, ink), url);
      }),
    ).then(() => {
      for (const k of [...cache.keys()].slice(0, Math.max(0, cache.size - CACHE_LIMIT))) cache.delete(k);
      if (!cancelled) setVersion((v) => v + 1);
    });
    return () => {
      cancelled = true;
    };
  }, [grid, ink]);

  const out: Record<string, string> = {};
  if (!ink) return out;
  for (const [key, t] of Object.entries(grid.tiles)) {
    const url = cache.get(cacheKey(key, t.seq, ink));
    if (url) out[key] = url;
  }
  return out;
}
