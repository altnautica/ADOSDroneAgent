// The Video screen's reads (cameras, encoder + radio config, latency probe)
// and its link-tuning draft. Every reply is coerced defensively: a field the
// agent does not send reads null, never a made-up value. Link tuning applies to
// the live data plane, so a draft only goes out through a slide, and never
// while the vehicle is armed (see `radio-gate`).

import { apiFetch } from "@/shared/api-fetch";
import type { VideoConfigPatch } from "@/shared/api/wfb";

import type { RadioWriteBlock } from "./radio-gate";

export const VIDEO_CAMERAS_PATH = "/api/video/cameras";
export const VIDEO_CONFIG_PATH = "/api/video/config";
export const VIDEO_LATENCY_PATH = "/api/video/latency";

export const LINK_PRESETS = ["conservative", "balanced", "aggressive"] as const;
export type LinkPreset = (typeof LINK_PRESETS)[number];

/** The FEC (k, n) pairs the tuning offers, strongest first. */
export const FEC_RATIOS: ReadonlyArray<readonly [number, number]> = [
  [8, 16],
  [8, 14],
  [8, 12],
  [8, 10],
  [4, 12],
];

export const MCS_INDICES: readonly number[] = [0, 1, 2, 3, 4, 5, 6, 7];

export interface VideoCamera {
  devicePath: string;
  type: string;
  label: string;
  width: number | null;
  height: number | null;
}

export interface VideoConfig {
  encoder: { codec: string | null; width: number | null; height: number | null; fps: number | null; bitrateKbps: number | null };
  radio: {
    channel: number | null;
    band: string | null;
    mcs: number | null;
    fecK: number | null;
    fecN: number | null;
    txPowerDbm: number | null;
    preset: string | null;
  };
  /** The adaptive bitrate / FEC controller's switch; null when not reported. */
  adaptive: boolean | null;
}

export interface VideoLatency {
  latestMs: number | null;
  ewmaMs: number | null;
  samples: number;
  source: string | null;
}

type Obj = Record<string, unknown>;

function obj(v: unknown): Obj | null {
  return v && typeof v === "object" && !Array.isArray(v) ? (v as Obj) : null;
}
function num(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}
function str(v: unknown): string | null {
  return typeof v === "string" && v.length > 0 ? v : null;
}

export function parseCameras(raw: unknown): VideoCamera[] {
  const list = obj(raw)?.cameras;
  if (!Array.isArray(list)) return [];
  const out: VideoCamera[] = [];
  for (const item of list) {
    const c = obj(item);
    const devicePath = str(c?.device_path);
    if (!c || !devicePath) continue;
    out.push({
      devicePath,
      type: str(c.type) ?? "",
      label: str(c.label) ?? devicePath,
      width: num(c.width),
      height: num(c.height),
    });
  }
  return out;
}

export function parseVideoConfig(raw: unknown): VideoConfig | null {
  const o = obj(raw);
  if (!o) return null;
  const enc = obj(o.encoder);
  const radio = obj(o.radio);
  const adaptive = obj(o.adaptive)?.adaptive_bitrate_enabled;
  return {
    encoder: {
      codec: str(enc?.codec),
      width: num(enc?.width),
      height: num(enc?.height),
      fps: num(enc?.fps),
      bitrateKbps: num(enc?.bitrate_kbps),
    },
    radio: {
      channel: num(radio?.channel),
      band: str(radio?.band),
      mcs: num(radio?.mcs_index),
      fecK: num(radio?.fec_k),
      fecN: num(radio?.fec_n),
      txPowerDbm: num(radio?.tx_power_dbm),
      preset: str(radio?.preset),
    },
    adaptive: typeof adaptive === "boolean" ? adaptive : null,
  };
}

export function parseLatency(raw: unknown): VideoLatency {
  const o = obj(raw);
  return {
    latestMs: num(o?.latency_ms),
    ewmaMs: num(o?.ewma_ms),
    samples: num(o?.samples) ?? 0,
    source: str(o?.source),
  };
}

export async function getVideoCameras(signal?: AbortSignal): Promise<VideoCamera[]> {
  return parseCameras(await apiFetch<unknown>(VIDEO_CAMERAS_PATH, { signal }));
}
export async function getVideoConfig(signal?: AbortSignal): Promise<VideoConfig | null> {
  return parseVideoConfig(await apiFetch<unknown>(VIDEO_CONFIG_PATH, { signal }));
}
export async function getVideoLatency(signal?: AbortSignal): Promise<VideoLatency> {
  return parseLatency(await apiFetch<unknown>(VIDEO_LATENCY_PATH, { signal }));
}

/** FEC redundancy as a whole percent of k, or null for an unusable pair. */
export function fecRedundancyPct(k: number | null, n: number | null): number | null {
  if (k == null || n == null || k <= 0 || n < k) return null;
  return Math.round(((n - k) / k) * 100);
}

/** The operator's pending tuning choices; an absent field is unchanged. */
export interface TuneDraft {
  preset?: LinkPreset;
  fec?: readonly [number, number];
  mcs?: number;
  auto?: boolean;
}

/** The patch a draft sends: only the fields that differ from the live
 *  config. Null when nothing would change. */
export function draftPatch(draft: TuneDraft, cfg: VideoConfig | null): VideoConfigPatch | null {
  const patch: VideoConfigPatch = {};
  const r = cfg?.radio;
  if (draft.preset && draft.preset !== r?.preset) patch.preset = draft.preset;
  if (draft.fec && (draft.fec[0] !== r?.fecK || draft.fec[1] !== r?.fecN)) {
    patch.fec_k = draft.fec[0];
    patch.fec_n = draft.fec[1];
  }
  if (draft.mcs != null && draft.mcs !== r?.mcs) patch.mcs = draft.mcs;
  if (draft.auto != null && draft.auto !== cfg?.adaptive) patch.auto = draft.auto;
  return Object.keys(patch).length > 0 ? patch : null;
}

/** Why the tuning slide is unavailable, or null when it may apply. */
export type TuneBlock = Exclude<RadioWriteBlock, null> | "noChange" | null;

export function tuneBlock(patch: VideoConfigPatch | null, gate: RadioWriteBlock): TuneBlock {
  if (gate) return gate;
  if (!patch) return "noChange";
  return null;
}
