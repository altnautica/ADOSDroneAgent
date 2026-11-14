// The sensors page's navigation sensor health (`GET /api/v1/sensors`
// `health`): the flight controller's optical flow and downward range, the
// scanning lidar and each camera, as the cockpit spec's `sensor-health.json`
// words in their state colours. Hidden when the agent sends no `health`.

import { useTranslation } from "react-i18next";

import {
  cameraWord,
  flowWord,
  lidarWord,
  rangeWord,
  type SensorsHealth,
  type SensorWord,
} from "@/shared/hud/sensor-health";

function Word({ w }: { w: SensorWord }) {
  return (
    <span className="whitespace-nowrap font-mono text-xs" style={{ color: w.hex }} data-state={w.state}>
      {w.text}
    </span>
  );
}

export function SensorHealthView({ health }: { health: SensorsHealth }) {
  const { t } = useTranslation();
  const parts: { label: string; words: SensorWord[] }[] = [
    { label: t("sensors.healthFlow"), words: [flowWord(health.flow)] },
    { label: t("sensors.healthRange"), words: [rangeWord(health.range)] },
    { label: t("sensors.healthLidar"), words: [lidarWord(health.lidar)] },
    { label: t("sensors.healthCameras"), words: health.cameras.map((c) => cameraWord(c)) },
  ];
  return (
    <section aria-label={t("sensors.health")} data-testid="sensor-health">
      <div className="mb-1 text-xs text-muted-foreground">{t("sensors.health")}</div>
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
        {parts.map((p) => (
          <div key={p.label}>
            <div className="text-xs text-muted-foreground">{p.label}</div>
            <div className="flex flex-col">
              {p.words.length > 0 ? p.words.map((w) => <Word key={w.text} w={w} />) : <span className="font-mono text-xs">{t("common.dash")}</span>}
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}
