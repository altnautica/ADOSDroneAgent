import { useTranslation } from "react-i18next";

import {
  ConfigEnumField,
  ConfigNumberField,
  ConfigToggle,
  ReadRow,
} from "@/components/settings/config-fields";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";
import { visionBackendOptions } from "@/lib/vision-backends";

export function VisionSettings() {
  const { t } = useTranslation();
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">{t("settingsMore.security.readingConfig")}</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        {t("settingsMore.vision.readError")}
      </div>
    );
  }

  const vision = config.data?.vision;
  if (!vision) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          {t("settingsMore.vision.unavailable")}
        </CardContent>
      </Card>
    );
  }

  const enabled = vision.enabled === true;

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <ConfigToggle
            configKey="vision.enabled"
            label={t("settingsMore.vision.engineLabel")}
            hint={t("settingsMore.vision.engineHint")}
            value={vision.enabled}
          />
        </CardContent>
      </Card>

      {enabled && (
        <>
          <Card>
            <CardContent className="pt-5 pb-5 space-y-4">
              <div>
                <div className="text-sm font-semibold">{t("settingsMore.vision.backendTitle")}</div>
                <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
                  {t("settingsMore.vision.backendHint")}
                </p>
              </div>
              <ConfigEnumField
                configKey="vision.backend"
                value={vision.backend}
                options={visionBackendOptions(t)}
                columns={2}
              />
            </CardContent>
          </Card>

          <Card>
            <CardContent className="pt-5 pb-5 space-y-5">
              <ConfigNumberField
                configKey="vision.confidence_threshold"
                id="vision-confidence"
                label={t("settingsMore.vision.confidenceLabel")}
                hint={t("settingsMore.vision.confidenceHint")}
                value={vision.confidence_threshold}
                integer={false}
                min={0}
                max={1}
              />
              <div className="border-t border-border pt-5">
                <ConfigNumberField
                  configKey="vision.models_cache_max_mb"
                  id="vision-cache"
                  label={t("settingsMore.vision.cacheLabel")}
                  hint={t("settingsMore.vision.cacheHint")}
                  value={vision.models_cache_max_mb}
                  integer
                  min={0}
                />
              </div>
              {vision.models_dir && (
                <div className="border-t border-border pt-5">
                  <ReadRow label={t("settingsMore.vision.modelsDir")} value={vision.models_dir} />
                </div>
              )}
            </CardContent>
          </Card>
        </>
      )}

      {/* Honest boundary: model selection is managed elsewhere. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1.5">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            {t("settingsMore.vision.managedElsewhere")}
          </div>
          <p className="text-xs text-muted-foreground leading-relaxed">
            {t("settingsMore.vision.managedElsewhereBody")}
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
