// The Map panel's rows from the world-engine `map_status` frame: compute
// placement, mapping memory used against its budget, the live voxel size,
// dropped keyframes, sweep-cloud points, mapped area and the upload or live
// stream progress. Every row carries a text value (never colour alone) and a
// tone; an absent field reads as a dash, never a made-up zero.

import { specText } from "@/shared/hud/spec-text";
import type { MapPlacement, MapStatus, StreamState, UploadState } from "@/shared/hud/nav-types";

/** `map_status` older than this is shown as stale. */
export const MAP_STATUS_FRESH_MS = 5000;
/** Memory share at which the row warns, and at which it is critical. */
export const MEMORY_WARN_FRACTION = 0.85;
export const MEMORY_CRIT_FRACTION = 0.95;

export type MapRowTone = "normal" | "good" | "warn" | "crit" | "muted";

export interface MapRow {
  id: "placement" | "memory" | "voxel" | "keyframes" | "points" | "area" | "session" | "upload" | "stream";
  label: string;
  value: string;
  tone: MapRowTone;
  /** 0–1 for rows drawn with a bar (memory, upload). */
  fraction?: number;
}

const DASH = "—";

const placementWord = (p: MapPlacement): string => specText(`map.status.placement.${p}`);
const uploadWord = (u: UploadState): string => specText(`map.status.upload.${u}`);
const streamWord = (st: StreamState): string => specText(`map.status.stream.${st}`);

/** `1.2 M`, `48.5 k`, `950`. */
export function compactCount(n: number): string {
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)} k`;
  return String(Math.round(n));
}

function memoryRow(used: number | null, budget: number | null): MapRow {
  if (used === null) return { id: "memory", label: specText("map.status.memory"), value: DASH, tone: "muted" };
  if (budget === null || budget <= 0) {
    return { id: "memory", label: specText("map.status.memory"), value: `${Math.round(used)} MB`, tone: "normal" };
  }
  const fraction = used / budget;
  const tone = fraction >= MEMORY_CRIT_FRACTION ? "crit" : fraction >= MEMORY_WARN_FRACTION ? "warn" : "normal";
  return {
    id: "memory",
    label: specText("map.status.memory"),
    value: `${Math.round(used)} / ${Math.round(budget)} MB · ${Math.round(fraction * 100)} %`,
    tone,
    fraction: Math.min(1, fraction),
  };
}

function uploadRow(upload: MapStatus["upload"]): MapRow | null {
  if (!upload) return null;
  const word = uploadWord(upload.state);
  const tone: MapRowTone = upload.state === "failed" ? "crit" : upload.state === "done" ? "good" : "normal";
  if (upload.bytesTotal > 0 && upload.state !== "idle") {
    const fraction = Math.min(1, upload.bytesDone / upload.bytesTotal);
    return {
      id: "upload",
      label: specText("map.status.upload"),
      value: `${word} ${Math.round(fraction * 100)} % · ${(upload.bytesDone / 1e6).toFixed(1)} / ${(upload.bytesTotal / 1e6).toFixed(1)} MB`,
      tone,
      fraction,
    };
  }
  return { id: "upload", label: specText("map.status.upload"), value: word, tone };
}

function streamRow(stream: MapStatus["stream"]): MapRow | null {
  if (!stream) return null;
  const word = streamWord(stream.state);
  const tone: MapRowTone = stream.state === "live" ? "good" : stream.state === "degraded" ? "warn" : "normal";
  const rate = stream.state === "live" || stream.state === "degraded"
    ? ` · ${((stream.bytesPerS * 8) / 1e6).toFixed(1)} Mbit/s`
    : "";
  return { id: "stream", label: specText("map.status.live_stream"), value: `${word}${rate}`, tone };
}

export function mapStatusRows(s: MapStatus): MapRow[] {
  const rows: MapRow[] = [
    {
      id: "placement",
      label: specText("map.status.placement"),
      value: s.placement ? placementWord(s.placement) : DASH,
      tone: s.placement ? "normal" : "muted",
    },
    memoryRow(s.memoryUsedMb, s.memoryBudgetMb),
    {
      id: "voxel",
      label: specText("map.status.voxel_size"),
      value: s.voxelM !== null ? `${Math.round(s.voxelM * 100)} cm` : DASH,
      tone: s.voxelM === null ? "muted" : "normal",
    },
    {
      id: "keyframes",
      label: specText("map.status.keyframes_dropped"),
      value: s.keyframesDropped !== null ? String(s.keyframesDropped) : DASH,
      tone: s.keyframesDropped === null ? "muted" : s.keyframesDropped > 0 ? "warn" : "normal",
    },
    {
      id: "points",
      label: specText("map.status.sweep_points"),
      value: s.sweepPoints !== null ? compactCount(s.sweepPoints) : DASH,
      tone: s.sweepPoints === null ? "muted" : "normal",
    },
    {
      id: "area",
      label: specText("map.status.area"),
      value: s.areaM2 !== null ? `${Math.round(s.areaM2).toLocaleString("en-US")} m²` : DASH,
      tone: s.areaM2 === null ? "muted" : "normal",
    },
    { id: "session", label: specText("map.status.session"), value: s.sessionId ?? DASH, tone: s.sessionId ? "normal" : "muted" },
  ];
  const up = uploadRow(s.upload);
  if (up) rows.push(up);
  const st = streamRow(s.stream);
  if (st) rows.push(st);
  return rows;
}
