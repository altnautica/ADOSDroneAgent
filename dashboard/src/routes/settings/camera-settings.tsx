import { useTranslation } from "react-i18next";

import { ConfigEnumField, ConfigToggle } from "@/components/settings/config-fields";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";

// The legal rotation values the camera config key accepts (clockwise degrees).
const ROTATION_OPTIONS = [
  { value: "0", label: "0°", descriptionKey: "settings.camera.rotation0" },
  { value: "90", label: "90°", descriptionKey: "settings.camera.rotation90" },
  { value: "180", label: "180°", descriptionKey: "settings.camera.rotation180" },
  { value: "270", label: "270°", descriptionKey: "settings.camera.rotation270" },
] as const;

export function CameraSettings() {
  const { t } = useTranslation();
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">{t("settings.shared.readingConfig")}</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        {t("settings.camera.readFailed")}
      </div>
    );
  }

  const camera = config.data?.video?.camera;
  if (!camera) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          {t("settings.camera.notExposed")}
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <div>
            <div className="text-sm font-semibold">{t("settings.camera.rotationTitle")}</div>
            <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
              {t("settings.camera.rotationHint")}
            </p>
          </div>
          <ConfigEnumField
            configKey="video.camera.rotation"
            value={
              camera.rotation != null
                ? (String(camera.rotation) as "0" | "90" | "180" | "270")
                : undefined
            }
            options={ROTATION_OPTIONS.map((o) => ({
              value: o.value,
              label: o.label,
              description: t(o.descriptionKey),
            }))}
            columns={2}
          />
          <div className="border-t border-border pt-5">
            <ConfigToggle
              configKey="video.camera.hflip"
              label={t("settings.camera.hflip")}
              hint={t("settings.camera.hflipHint")}
              value={camera.hflip}
            />
          </div>
          <div className="border-t border-border pt-5">
            <ConfigToggle
              configKey="video.camera.vflip"
              label={t("settings.camera.vflip")}
              hint={t("settings.camera.vflipHint")}
              value={camera.vflip}
            />
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
