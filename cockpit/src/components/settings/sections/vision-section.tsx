// Vision: the on-board detection engine switch, its inference backend, the
// confidence threshold (stored as a 0..1 fraction, edited as a percent) and
// the model cache budget. Plain config-leaf writes.

import { useTranslation } from "react-i18next";

import { EmptyNote, Row } from "@/components/ui/data";
import { SelectSettingRow, SwitchSettingRow } from "@/components/ui/setting-row";
import {
  CACHE_BOUNDS,
  VISION_BACKENDS,
  VISION_KEYS,
  confidenceToPct,
  pctToConfidence,
  toBackend,
  type VisionBackend,
} from "@/lib/settings/vision";
import type { AgentConfig } from "@/shared/api/types";

import { LeafNumberRow, SectionGroup } from "./section-kit";
import { useAgentConfig, useLeafWriter } from "./section-hooks";

export function VisionSection() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();
  const { save } = useLeafWriter();

  if (!ready) return <EmptyNote>{t("settingsMore.security.readingConfig")}</EmptyNote>;
  if (config == null) return <EmptyNote>{t("settingsMore.vision.readError")}</EmptyNote>;
  const vision = get<AgentConfig["vision"] | null>("vision", null);
  if (!vision) return <EmptyNote>{t("settingsMore.vision.unavailable")}</EmptyNote>;

  const enabled = vision.enabled === true;
  const backend = toBackend(vision.backend);

  return (
    <div className="flex flex-col gap-[0.8rem]">
      <SwitchSettingRow
        title={t("settingsMore.vision.engineLabel")}
        summary={t("settingsMore.vision.engineHint")}
        checked={enabled}
        onChange={(next) => void save(VISION_KEYS.enabled, next)}
      />

      {enabled ? (
        <>
          <SectionGroup title={t("settingsMore.vision.backendTitle")}>
            <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
              {t("settingsMore.vision.backendHint")}
            </p>
            <SelectSettingRow<VisionBackend>
              title={t("settingsMore.vision.backendTitle")}
              summary={t(`settingsMore.vision.backends.${backend}.description` as const)}
              options={VISION_BACKENDS}
              value={backend}
              optionLabel={(o) => t(`settingsMore.vision.backends.${o}.label` as const)}
              onSelect={(o) => {
                if (o !== vision.backend) void save(VISION_KEYS.backend, o);
              }}
            />
          </SectionGroup>

          <SectionGroup>
            <LeafNumberRow
              title={t("settingsMore.vision.confidenceLabel")}
              summary={t("settingsMore.vision.confidenceHint")}
              value={confidenceToPct(vision.confidence_threshold ?? 0)}
              enabled={vision.confidence_threshold != null}
              min={0}
              max={100}
              format={(pct) => `${pct}%`}
              onCommit={(pct) => save(VISION_KEYS.confidence, pctToConfidence(pct))}
            />
            <LeafNumberRow
              title={t("settingsMore.vision.cacheLabel")}
              summary={t("settingsMore.vision.cacheHint")}
              value={vision.models_cache_max_mb ?? CACHE_BOUNDS.min}
              enabled={vision.models_cache_max_mb != null}
              min={CACHE_BOUNDS.min}
              max={CACHE_BOUNDS.max}
              step={CACHE_BOUNDS.step}
              onCommit={(next) => save(VISION_KEYS.cacheMb, next)}
            />
            {vision.models_dir ? <Row label={t("settingsMore.vision.modelsDir")} value={vision.models_dir} /> : null}
          </SectionGroup>
        </>
      ) : null}

      <SectionGroup title={t("settingsMore.vision.managedElsewhere")}>
        <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
          {t("settingsMore.vision.managedElsewhereBody")}
        </p>
      </SectionGroup>
    </div>
  );
}
