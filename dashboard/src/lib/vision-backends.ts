// The vision inference backends the native engine selects between. Mirrors the
// `vision.backend` enum in schemas/agent-config.schema.json (a unit test holds
// the two equal); the config write refuses any other value. Labels and
// descriptions live in the locale under `settingsMore.vision.backends.<value>`.

import type { TFunction } from "i18next";

export const VISION_BACKENDS = ["auto", "rknn", "hailo", "onnx", "mock"] as const;

export type VisionBackend = (typeof VISION_BACKENDS)[number];

export function visionBackendOptions(
  t: TFunction,
): ReadonlyArray<{ value: VisionBackend; label: string; description: string }> {
  return VISION_BACKENDS.map((value) => ({
    value,
    label: t(`settingsMore.vision.backends.${value}.label`),
    description: t(`settingsMore.vision.backends.${value}.description`),
  }));
}
