// When the native engine runs detection. Mirrors the `vision.detection_demand`
// enum in schemas/agent-config.schema.json (a unit test holds the two equal);
// the config write refuses any other value. Labels and descriptions live in
// the locale under `settingsMore.vision.demandModes.<value>`.

import type { TFunction } from "i18next";

export const DETECTION_DEMAND_MODES = ["on_demand", "always"] as const;

export type DetectionDemandMode = (typeof DETECTION_DEMAND_MODES)[number];

export function detectionDemandOptions(
  t: TFunction,
): ReadonlyArray<{ value: DetectionDemandMode; label: string; description: string }> {
  return DETECTION_DEMAND_MODES.map((value) => ({
    value,
    label: t(`settingsMore.vision.demandModes.${value}.label`),
    description: t(`settingsMore.vision.demandModes.${value}.description`),
  }));
}
