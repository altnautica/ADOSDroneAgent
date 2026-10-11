import { useTranslation } from "react-i18next";

import { Card, CardContent } from "@/components/ui/card";
import {
  ConfigNumberField,
  ConfigToggle,
} from "@/components/settings/config-fields";
import { useConfig } from "@/hooks/use-config";

/** Auto-recovery reconcilers the operator can tune, plus an honest note about
 * the always-on guardians that carry no config knob. Every write is a
 * PUT /api/config key that the agent actually accepts. */
export function SelfHealSettings() {
  const { t } = useTranslation();
  const config = useConfig();

  if (config.isLoading) {
    return (
      <p className="text-[11px] text-muted-foreground/70">{t("settingsMore.security.readingConfig")}</p>
    );
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        {t("settingsMore.selfHeal.readError")}
      </div>
    );
  }

  const selfheal = config.data?.network?.wifi_selfheal;
  const usbRecovery = config.data?.video?.usb_recovery;

  return (
    <div className="space-y-6">
      {/* Onboard-WiFi self-heal — always present in the config model. */}
      {selfheal ? (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-5">
            <div>
              <div className="text-sm font-semibold">{t("settingsMore.selfHeal.wifiTitle")}</div>
              <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
                {t("settingsMore.selfHeal.wifiBody")}
              </p>
            </div>
            <ConfigToggle
              configKey="network.wifi_selfheal.enabled"
              label={t("settingsMore.selfHeal.wifiEnabledLabel")}
              hint={t("settingsMore.selfHeal.wifiEnabledHint")}
              value={selfheal.enabled}
            />
            <div className="border-t border-border pt-5">
              <ConfigNumberField
                configKey="network.wifi_selfheal.fail_threshold"
                id="selfheal-fail-threshold"
                label={t("settingsMore.selfHeal.failThresholdLabel")}
                hint={t("settingsMore.selfHeal.failThresholdHint")}
                value={selfheal.fail_threshold}
                integer
                min={1}
              />
            </div>
            <ConfigNumberField
              configKey="network.wifi_selfheal.cooldown_s"
              id="selfheal-cooldown"
              label={t("settingsMore.selfHeal.cooldownLabel")}
              hint={t("settingsMore.selfHeal.cooldownHint")}
              value={selfheal.cooldown_s}
              integer
              min={0}
            />
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
            {t("settingsMore.selfHeal.wifiUnavailable")}
          </CardContent>
        </Card>
      )}

      {/* Camera USB-recovery — a video-domain reconciler, present in config. */}
      {usbRecovery && (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-5">
            <div>
              <div className="text-sm font-semibold">{t("settingsMore.selfHeal.usbTitle")}</div>
              <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
                {t("settingsMore.selfHeal.usbBody")}
              </p>
            </div>
            <ConfigToggle
              configKey="video.usb_recovery.enabled"
              label={t("settingsMore.selfHeal.usbEnabledLabel")}
              hint={t("settingsMore.selfHeal.usbEnabledHint")}
              value={usbRecovery.enabled}
            />
            <div className="border-t border-border pt-5">
              <ConfigToggle
                configKey="video.usb_recovery.allow_ppps"
                label={t("settingsMore.selfHeal.pppsLabel")}
                hint={t("settingsMore.selfHeal.pppsHint")}
                value={usbRecovery.allow_ppps}
              />
            </div>
          </CardContent>
        </Card>
      )}

      {/* Honest disclosure: the deeper guardians have no operator knob. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1.5">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            {t("settingsMore.selfHeal.guardiansTitle")}
          </div>
          <p className="text-xs text-muted-foreground leading-relaxed">
            {t("settingsMore.selfHeal.guardiansBody")}
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
