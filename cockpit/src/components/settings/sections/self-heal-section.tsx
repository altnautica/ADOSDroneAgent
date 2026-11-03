// Self-heal: the auto-recovery reconcilers the operator can tune (onboard
// Wi-Fi re-association, camera USB recovery), plus a note about the always-on
// guardians that carry no knob. Enabling the USB power-cycle waits behind a
// guarded button; every other change is a direct config-leaf write.

import { useTranslation } from "react-i18next";

import { EmptyNote } from "@/components/ui/data";
import { SwitchSettingRow } from "@/components/ui/setting-row";
import { COOLDOWN_BOUNDS, FAIL_THRESHOLD_BOUNDS, SELF_HEAL_KEYS, selfHealNeedsGuard } from "@/lib/settings/self-heal";
import type { AgentConfig } from "@/shared/api/types";

import { LeafNumberRow, PendingLeafBar, SectionGroup } from "./section-kit";
import { useAgentConfig, useGuardedLeaf } from "./section-hooks";

export function SelfHealSection() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();
  const leaf = useGuardedLeaf();

  if (!ready) return <EmptyNote>{t("settingsMore.security.readingConfig")}</EmptyNote>;
  if (config == null) return <EmptyNote>{t("settingsMore.selfHeal.readError")}</EmptyNote>;

  type Net = NonNullable<AgentConfig["network"]>;
  type Video = NonNullable<AgentConfig["video"]>;
  const selfheal = get<Net["wifi_selfheal"] | null>("network.wifi_selfheal", null);
  const usb = get<Video["usb_recovery"] | null>("video.usb_recovery", null);
  const write = (key: string, value: boolean | number) =>
    leaf.request({ key, value }, selfHealNeedsGuard(key, value));
  const pppsPending = leaf.pending?.key === SELF_HEAL_KEYS.powerCycle;

  return (
    <div className="flex flex-col gap-[0.8rem]">
      <SectionGroup title={t("settingsMore.selfHeal.wifiTitle")}>
        {selfheal ? (
          <>
            <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
              {t("settingsMore.selfHeal.wifiBody")}
            </p>
            <SwitchSettingRow
              title={t("settingsMore.selfHeal.wifiEnabledLabel")}
              summary={t("settingsMore.selfHeal.wifiEnabledHint")}
              checked={selfheal.enabled === true}
              onChange={(next) => void write(SELF_HEAL_KEYS.wifiEnabled, next)}
            />
            <LeafNumberRow
              title={t("settingsMore.selfHeal.failThresholdLabel")}
              summary={t("settingsMore.selfHeal.failThresholdHint")}
              value={selfheal.fail_threshold ?? FAIL_THRESHOLD_BOUNDS.min}
              enabled={selfheal.fail_threshold != null}
              min={FAIL_THRESHOLD_BOUNDS.min}
              max={FAIL_THRESHOLD_BOUNDS.max}
              onCommit={(next) => write(SELF_HEAL_KEYS.failThreshold, next)}
            />
            <LeafNumberRow
              title={t("settingsMore.selfHeal.cooldownLabel")}
              summary={t("settingsMore.selfHeal.cooldownHint")}
              value={selfheal.cooldown_s ?? COOLDOWN_BOUNDS.min}
              enabled={selfheal.cooldown_s != null}
              min={COOLDOWN_BOUNDS.min}
              max={COOLDOWN_BOUNDS.max}
              step={5}
              onCommit={(next) => write(SELF_HEAL_KEYS.cooldown, next)}
            />
          </>
        ) : (
          <EmptyNote>{t("settingsMore.selfHeal.wifiUnavailable")}</EmptyNote>
        )}
      </SectionGroup>

      {usb ? (
        <SectionGroup title={t("settingsMore.selfHeal.usbTitle")}>
          <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
            {t("settingsMore.selfHeal.usbBody")}
          </p>
          <SwitchSettingRow
            title={t("settingsMore.selfHeal.usbEnabledLabel")}
            summary={t("settingsMore.selfHeal.usbEnabledHint")}
            checked={usb.enabled === true}
            onChange={(next) => void write(SELF_HEAL_KEYS.usbEnabled, next)}
          />
          <SwitchSettingRow
            title={t("settingsMore.selfHeal.pppsLabel")}
            summary={t("settingsMore.selfHeal.pppsHint")}
            checked={pppsPending ? true : usb.allow_ppps === true}
            enabled={!pppsPending}
            onChange={(next) => void write(SELF_HEAL_KEYS.powerCycle, next)}
          />
          {pppsPending ? (
            <PendingLeafBar
              prompt={t("cockpit.settingsSections.selfHeal.pppsPrompt")}
              fireLabel={t("cockpit.settingsSections.selfHeal.pppsFire")}
              holdLabel={t("cockpit.settingsSections.selfHeal.pppsHold")}
              onFire={leaf.fire}
              onCancel={leaf.cancel}
              disabled={leaf.busy}
            />
          ) : null}
        </SectionGroup>
      ) : null}

      <SectionGroup title={t("settingsMore.selfHeal.guardiansTitle")}>
        <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
          {t("settingsMore.selfHeal.guardiansBody")}
        </p>
      </SectionGroup>
    </div>
  );
}
