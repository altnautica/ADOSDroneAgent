// Hotspot: the node's own setup access point. Turning it off commits at once.
// Turning it on takes over the Wi-Fi radio, so while a Wi-Fi client link is
// up it drops that link and waits behind a slide; with no client link it
// commits at once. Writes go through the batch apply endpoint (residual
// Python); a refused or failed write puts the switch back.

import { useCallback, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { EmptyNote } from "@/components/ui/data";
import { SwitchSettingRow } from "@/components/ui/setting-row";
import { sectionVerdict } from "@/lib/settings/apply-form";
import { hotspotToggle, wifiClientConnected } from "@/lib/settings/hotspot";
import { apiFetch } from "@/shared/api-fetch";
import type { SetupStatus } from "@/shared/agent-api.generated";
import { postApply } from "@/shared/api/apply-actions";
import { hotspotSharesRadio, resolveHotspotSsid } from "@/shared/api/hotspot";
import { toast } from "@/shared/api/toast";
import { getGsNetwork } from "@/shared/api/uplink";
import { getWifiStatus } from "@/shared/api/wifi";
import "@/shared/i18n";
import { useProfile } from "@/shared/use-profile";
import { useResource } from "@/shared/use-resource";
import { ResidualMissingNote, SectionGroup, SlideConfirmDialog } from "./section-kit";
import { toastActionError } from "./section-hooks";

export function HotspotSection() {
  const { t } = useTranslation();
  const setup = useResource(
    useCallback((s: AbortSignal) => apiFetch<SetupStatus>("/api/v1/setup/status", { signal: s }), []),
    5_000,
  );
  const wifi = useResource(
    useCallback(() => getWifiStatus(), []),
    5_000,
  );

  const agentEnabled = setup.data?.network?.hotspot_enabled ?? false;
  const [enabled, setEnabled] = useState(agentEnabled);
  const [busy, setBusy] = useState(false);
  const [pendingEnable, setPendingEnable] = useState(false);

  // Follow the agent's value whenever it changes.
  const [seededFrom, setSeededFrom] = useState(agentEnabled);
  if (seededFrom !== agentEnabled) {
    setSeededFrom(agentEnabled);
    setEnabled(agentEnabled);
  }

  // A ground station runs its AP on its own interface (never the WFB radio)
  // under its own name; its live AP leg names both. While the profile is
  // still loading, nothing but that leg may name the network.
  const profile = useProfile();
  const isGround = profile === "ground_station";
  const gsNet = useResource(
    useCallback(() => getGsNetwork(), []),
    10_000,
    { enabled: isGround },
  );
  const gsAp = isGround ? gsNet.data?.ap : null;
  const ssid = resolveHotspotSsid(setup.data?.network, gsAp, profile === null ? null : isGround);

  const commit = async (next: boolean) => {
    setEnabled(next);
    setBusy(true);
    try {
      const res = await postApply({ network: { hotspot_enabled: next } });
      const verdict = sectionVerdict(res, "network");
      if (verdict.ok) {
        toast.ok(verdict.message ?? t("settings.network.hotspotUpdated"));
      } else {
        setEnabled(!next);
        toast.err(verdict.message ?? t("settings.network.hotspotFailed"));
      }
    } catch (err) {
      setEnabled(!next);
      toastActionError(err, t("cockpit.settingsSections.residualMissing"));
    } finally {
      setBusy(false);
      setup.refresh();
    }
  };

  const onToggle = (next: boolean) => {
    const step = hotspotToggle(next, wifiClientConnected(wifi.data));
    if (step.kind === "confirm") setPendingEnable(true);
    else void commit(step.enabled);
  };

  if (!setup.ready) return <EmptyNote>{t("cockpit.settingsSections.loading")}</EmptyNote>;
  if (setup.status === 501) return <ResidualMissingNote />;

  return (
    <div className="flex flex-col gap-[0.6rem]">
      <SectionGroup>
        <SwitchSettingRow
          title={t("settings.network.hotspotTitle")}
          checked={enabled}
          onChange={onToggle}
          enabled={!busy && setup.data != null}
        />
        <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
          {ssid.known ? (
            <Trans
              i18nKey="settings.network.hotspotHintNamed"
              values={{ ssid: ssid.ssid }}
              components={{ code: <span className="font-mono" /> }}
            />
          ) : (
            t("settings.network.hotspotHintUnnamed")
          )}
          {ssid.iface ? (
            <>
              {" "}
              <Trans
                i18nKey="settings.network.hotspotHintIface"
                values={{ iface: ssid.iface }}
                components={{ code: <span className="font-mono" /> }}
              />
            </>
          ) : null}
        </p>
        {hotspotSharesRadio(gsAp) ? (
          <p className="px-[0.75rem] text-[0.8rem] text-warn">{t("settings.network.hotspotWarning")}</p>
        ) : null}
      </SectionGroup>

      {pendingEnable ? (
        <SlideConfirmDialog
          title={t("settings.network.confirmTitle")}
          slideLabel={t("cockpit.settingsSections.hotspot.applySlide")}
          onDismiss={() => setPendingEnable(false)}
          onConfirmed={() => {
            setPendingEnable(false);
            void commit(true);
          }}
        >
          <Trans
            i18nKey={ssid.known ? "settings.network.confirmBodyNamed" : "settings.network.confirmBodyUnnamed"}
            values={{
              current: wifi.data?.ssid ?? t("settings.network.aWifiNetwork"),
              ssid: ssid.known ? ssid.ssid : "",
            }}
            components={{
              current: <span className="font-mono font-medium" />,
              code: <span className="font-mono" />,
            }}
          />
        </SlideConfirmDialog>
      ) : null}
    </div>
  );
}
