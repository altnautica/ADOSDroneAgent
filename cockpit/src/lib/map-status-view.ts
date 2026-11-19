// The Map panel's rows from Navigation's `map_status` frame: compute
// placement, mapping memory used against its budget, the live voxel size,
// sweep-cloud points, mapped area, the pose provider and the FC IMU feed.
// Every row carries a text value (never colour alone) and a tone; an absent
// field reads as a dash, never a made-up zero.

import { specText } from "@/shared/hud/spec-text";
import type { ImuFeed, MapPlacement, MapStatus, PoseProvider } from "@/shared/hud/nav-types";

/** `map_status` older than this is shown as stale. */
export const MAP_STATUS_FRESH_MS = 5000;
/** Memory share at which the row warns, and at which it is critical. */
export const MEMORY_WARN_FRACTION = 0.85;
export const MEMORY_CRIT_FRACTION = 0.95;

export type MapRowTone = "normal" | "good" | "warn" | "crit" | "muted";

export interface MapRow {
  id: "placement" | "memory" | "voxel" | "points" | "area" | "pose" | "imu";
  label: string;
  value: string;
  tone: MapRowTone;
  /** 0–1 for rows drawn with a bar (memory). */
  fraction?: number;
}

const DASH = "—";

const placementWord = (p: MapPlacement): string => specText(`map.status.placement.${p}`);
const providerWord = (p: PoseProvider): string => specText(`map.status.pose_provider.${p}`);

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

function imuRow(feed: ImuFeed): MapRow {
  const word = specText(`map.status.imu_feed.${feed.state}`);
  const tone: MapRowTone = feed.state === "lost" ? "crit" : feed.state === "degraded" ? "warn" : "good";
  const rate = feed.rateHz !== null ? ` · ${Math.round(feed.rateHz)} Hz` : "";
  return { id: "imu", label: specText("map.status.imu_feed"), value: `${word}${rate}`, tone };
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
    {
      id: "pose",
      label: specText("map.status.pose_provider"),
      value: s.poseProvider ? providerWord(s.poseProvider) : DASH,
      tone: s.poseProvider ? "normal" : "muted",
    },
  ];
  if (s.imuFeed) rows.push(imuRow(s.imuFeed));
  return rows;
}
