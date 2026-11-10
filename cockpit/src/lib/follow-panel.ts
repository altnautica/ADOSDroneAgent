// The range panel's content (13-hud-v2 §8.1) from the latest `follow.state`:
// the big range and phase word, the ±2σ interval bar, the geometry line, and
// the range cues with the pipeline latency and the re-acquire mode. Height
// above ground is the bottom row's alone: one AGL on screen. Every value
// reads `—` once the report is older than 2 s, and the phase word then reads
// STALE. Pure, so the panel is a thin view.

import {
  bearing3,
  intervalBar,
  LATENCY_WARN_MS,
  PHASE_TONE,
  RANGE_CUE_ORDER,
  type IntervalBar,
  type PhaseTone,
} from "@/shared/hud/follow-chrome";
import type { FollowState } from "@/shared/hud/nav-types";
import { FOLLOW_FRESH_MS, formatRange, rangeInterval } from "@/shared/hud/target-state";
import { specText } from "@/shared/hud/spec-text";

const DASH = "—";

export interface FollowPanelModel {
  stale: boolean;
  range: string;
  /** `±0.6` (2σ, one decimal), or null with no sigma. */
  plusMinus: string | null;
  phase: string;
  phaseTone: PhaseTone | "warn";
  bar: IntervalBar | null;
  interval: string | null;
  geometry: string;
  cues: { label: string; lit: boolean }[];
  latency: string;
  latencyWarn: boolean;
  /** The reacquire mode's word (`follow.mode.*`), or null. */
  mode: string | null;
}

export function followPanelModel(f: FollowState | null, ageMs: number): FollowPanelModel {
  const stale = f === null || ageMs > FOLLOW_FRESH_MS;
  const v = stale ? null : f;
  const rangeM = v?.rangeM ?? null;
  const sigma = v?.rangeSigmaM ?? null;
  const lit = new Set(v?.rangeCues.map((c) => c.kind) ?? []);
  const dep = (s: string) => specText("follow.geo.depression", { v: s });
  const brg = (s: string) => specText("follow.geo.bearing", { v: s });
  const geo = v
    ? [
        dep(v.depressionDeg !== null ? `${Math.round(v.depressionDeg)}°` : DASH),
        brg(v.bearingDeg !== null ? `${bearing3(v.bearingDeg)}°` : DASH),
        v.targetSpeedMps !== null ? `${v.targetSpeedMps.toFixed(1)} m/s` : `${DASH} m/s`,
      ].join(" · ")
    : `${dep(DASH)} · ${brg(DASH)} · ${DASH} m/s`;
  return {
    stale,
    range: rangeM !== null ? formatRange(rangeM) : `${DASH} m`,
    plusMinus: rangeM !== null && sigma !== null ? `±${(2 * sigma).toFixed(1)}` : null,
    phase: specText(`follow.phase.${v ? v.phase : "stale"}`),
    phaseTone: v ? PHASE_TONE[v.phase] : "warn",
    bar: rangeM !== null ? intervalBar(rangeM, sigma) : null,
    interval: rangeM !== null && sigma !== null ? rangeInterval(rangeM, sigma) : null,
    geometry: geo,
    cues: RANGE_CUE_ORDER.map((k) => ({ label: specText(`follow.cue.${k}`), lit: lit.has(k) })),
    latency: specText("follow.latency", { v: v?.pipelineLatencyMs != null ? `${Math.round(v.pipelineLatencyMs)} ms` : DASH }),
    latencyWarn: (v?.pipelineLatencyMs ?? 0) > LATENCY_WARN_MS,
    mode: v?.reacquireMode === "smart" || v?.reacquireMode === "strict" ? specText(`follow.mode.${v.reacquireMode}`) : null,
  };
}

/** The compact single line: `10.2 m ±0.6`. */
export function compactFollowLine(m: FollowPanelModel): string {
  return m.plusMinus ? `${m.range} ${m.plusMinus}` : m.range;
}
