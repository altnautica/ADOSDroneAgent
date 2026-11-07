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
import { detectionDemandOptions } from "@/lib/vision-demand";

/** The detector's per-camera rate when `detector.rate_hz` is absent. */
const DEFAULT_DETECTOR_RATE_HZ = 3;

/** The depth model's threads when `depth.threads` is absent. */
const DEFAULT_DEPTH_THREADS = 2;

/** The depth pace when `depth.rate_hz` is absent: on the Rockchip
 *  accelerator, and on the CPU runtime. */
const DEPTH_RATE_ACCEL_HZ = 5;
const DEPTH_RATE_CPU_HZ = 2;

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

          <Card>
            <CardContent className="pt-5 pb-5 space-y-5">
              <div className="space-y-4">
                <div>
                  <div className="text-sm font-semibold">{t("settingsMore.vision.demandTitle")}</div>
                  <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
                    {t("settingsMore.vision.demandHint")}
                  </p>
                </div>
                <ConfigEnumField
                  configKey="vision.detection_demand"
                  value={vision.detection_demand}
                  options={detectionDemandOptions(t)}
                  columns={2}
                />
              </div>
              <div className="border-t border-border pt-5">
                <ConfigNumberField
                  configKey="vision.designate_hold_s"
                  id="vision-designate-hold"
                  label={t("settingsMore.vision.designateHoldLabel")}
                  hint={t("settingsMore.vision.designateHoldHint")}
                  value={vision.designate_hold_s}
                  integer={false}
                  min={0}
                  max={60}
                />
              </div>
              <div className="border-t border-border pt-5">
                <ConfigNumberField
                  configKey="vision.max_box_area_frac"
                  id="vision-max-box-area"
                  label={t("settingsMore.vision.maxBoxAreaLabel")}
                  hint={t("settingsMore.vision.maxBoxAreaHint")}
                  value={vision.max_box_area_frac}
                  integer={false}
                  min={0}
                  max={1}
                />
              </div>
              <div className="border-t border-border pt-5">
                <ConfigNumberField
                  configKey="vision.decode_threads"
                  id="vision-decode-threads"
                  label={t("settingsMore.vision.decodeThreadsLabel")}
                  hint={t("settingsMore.vision.decodeThreadsHint")}
                  value={vision.decode_threads}
                  integer
                  min={1}
                />
              </div>
            </CardContent>
          </Card>

          <Card>
            <CardContent className="pt-5 pb-5 space-y-5">
              <div className="text-sm font-semibold">{t("settingsMore.vision.detectorTitle")}</div>
              {vision.detector ? (
                <>
                  <ConfigNumberField
                    configKey="vision.detector.rate_hz"
                    id="vision-detector-rate"
                    label={t("settingsMore.vision.detectorRateLabel")}
                    hint={t("settingsMore.vision.detectorRateHint")}
                    value={vision.detector.rate_hz ?? DEFAULT_DETECTOR_RATE_HZ}
                    integer={false}
                    min={0}
                  />
                  <div className="border-t border-border pt-5">
                    <ConfigNumberField
                      configKey="vision.detector.threads"
                      id="vision-detector-threads"
                      label={t("settingsMore.vision.detectorThreadsLabel")}
                      hint={t("settingsMore.vision.detectorThreadsHint")}
                      value={vision.detector.threads}
                      integer
                      min={1}
                    />
                  </div>
                </>
              ) : (
                <p className="text-xs text-muted-foreground leading-relaxed">
                  {t("settingsMore.vision.noDetector")}
                </p>
              )}
            </CardContent>
          </Card>

          <Card>
            <CardContent className="pt-5 pb-5 space-y-5">
              <ConfigToggle
                configKey="vision.depth.enabled"
                label={t("settingsMore.vision.depthLabel")}
                hint={t("settingsMore.vision.depthHint")}
                value={vision.depth?.enabled ?? true}
              />
              {(vision.depth?.enabled ?? true) && (
                <>
                  <div className="border-t border-border pt-5">
                    <ConfigNumberField
                      configKey="vision.depth.rate_hz"
                      id="vision-depth-rate"
                      label={t("settingsMore.vision.depthRateLabel")}
                      hint={t("settingsMore.vision.depthRateHint")}
                      value={
                        vision.depth?.rate_hz ??
                        (vision.backend === "rknn" ? DEPTH_RATE_ACCEL_HZ : DEPTH_RATE_CPU_HZ)
                      }
                      integer={false}
                      min={0.1}
                    />
                  </div>
                  <div className="border-t border-border pt-5">
                    <ConfigNumberField
                      configKey="vision.depth.threads"
                      id="vision-depth-threads"
                      label={t("settingsMore.vision.depthThreadsLabel")}
                      hint={t("settingsMore.vision.depthThreadsHint")}
                      value={vision.depth?.threads ?? DEFAULT_DEPTH_THREADS}
                      integer
                      min={1}
                    />
                  </div>
                </>
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
