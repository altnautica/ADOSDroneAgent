// The Vision settings page model. The inference backends mirror the
// `vision.backend` enum in schemas/agent-config.schema.json (the config write
// refuses any other value); labels live in the locale under
// `settingsMore.vision.backends.<value>`. The confidence thresholds (the crop
// pass's and the whole frame's) are 0.05..1 fractions edited as whole
// percents; 0.05 is the detector's decode floor.

export const VISION_BACKENDS = ["auto", "rknn", "hailo", "onnx", "mock"] as const;
export type VisionBackend = (typeof VISION_BACKENDS)[number];

export const VISION_KEYS = {
  enabled: "vision.enabled",
  backend: "vision.backend",
  confidence: "vision.confidence_threshold",
  wholeFrameConf: "vision.whole_frame_conf",
  cacheMb: "vision.models_cache_max_mb",
} as const;

export const CACHE_BOUNDS = { min: 0, max: 1_000_000, step: 50 } as const;

/** The confidence thresholds' range as whole percents. */
export const CONFIDENCE_PCT_BOUNDS = { min: 5, max: 100 } as const;

export function toBackend(v: unknown): VisionBackend {
  return (VISION_BACKENDS as readonly unknown[]).includes(v) ? (v as VisionBackend) : "auto";
}

/** The stored fraction as a whole percent, clamped to 0..100. */
export function confidenceToPct(v: number): number {
  if (!Number.isFinite(v)) return 0;
  return Math.min(100, Math.max(0, Math.round(v * 100)));
}

/** A whole percent as the fraction the config stores. */
export function pctToConfidence(pct: number): number {
  return Math.round(Math.min(100, Math.max(0, pct))) / 100;
}
