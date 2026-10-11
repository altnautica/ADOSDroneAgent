import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { NetworkUplinkPanel } from "@/components/network/network-uplink-panel";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { WifiPanel } from "@/components/wifi/wifi-panel";
import { useStatus } from "@/hooks/use-status";
import { postApply } from "@/lib/apply-actions";
import { toast, toastFromError } from "@/lib/toast";
import { getWifiStatus } from "@/lib/wifi";
import { resolveHotspotSsid } from "@/lib/hotspot";

export function NetworkSettings() {
  const { t } = useTranslation();
  const status = useStatus();
  const wifiStatus = useQuery({
    queryKey: ["wifi", "status"],
    queryFn: () => getWifiStatus(),
    refetchInterval: 5_000,
  });

  const initialHotspot = status.data?.network?.hotspot_enabled ?? false;
  const [hotspot, setHotspot] = useState(initialHotspot);
  const [busy, setBusy] = useState(false);
  const [pendingEnable, setPendingEnable] = useState(false);

  useEffect(() => {
    if (status.data) {
      setHotspot(status.data.network?.hotspot_enabled ?? false);
    }
  }, [status.data]);

  const hotspotSsid = useMemo(
    () => resolveHotspotSsid(status.data?.network),
    [status.data?.network],
  );

  async function commitHotspot(next: boolean) {
    setBusy(true);
    try {
      const res = await postApply({ network: { hotspot_enabled: next } });
      const section = res.sections.network;
      if (!res.overall || !section?.ok) {
        setHotspot(!next);
        toast.err(section?.message ?? t("settings.network.hotspotFailed"));
      } else {
        toast.ok(section.message || t("settings.network.hotspotUpdated"));
      }
    } catch (err) {
      setHotspot(!next);
      toastFromError(err, t("settings.network.hotspotFailed"));
    } finally {
      setBusy(false);
    }
  }

  function onToggle(next: boolean) {
    // Disabling the AP is never destructive — just turn it off.
    if (!next) {
      setHotspot(false);
      void commitHotspot(false);
      return;
    }
    // Enabling the AP commandeers wlan0. If the rig has an active
    // Wi-Fi client link the connection drops; gate behind a confirm.
    const clientConnected = wifiStatus.data?.connected === true;
    if (clientConnected) {
      setPendingEnable(true);
      return;
    }
    setHotspot(true);
    void commitHotspot(true);
  }

  async function confirmEnable() {
    setHotspot(true);
    setPendingEnable(false);
    await commitHotspot(true);
  }

  return (
    <div className="space-y-6">
      <NetworkUplinkPanel />

      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="flex items-start justify-between gap-4">
            <div className="space-y-2 min-w-0">
              <div className="flex items-center gap-2 text-sm font-medium">
                {t("settings.network.hotspotTitle")}
                <RiskBadge tone="auto" />
              </div>
              <div className="text-xs text-muted-foreground leading-relaxed">
                {hotspotSsid.known ? (
                  <Trans
                    i18nKey="settings.network.hotspotHintNamed"
                    values={{ ssid: hotspotSsid.ssid }}
                    components={{ code: <span className="font-mono" /> }}
                  />
                ) : (
                  t("settings.network.hotspotHintUnnamed")
                )}
              </div>
              <div className="text-xs text-warn">
                {t("settings.network.hotspotWarning")}
              </div>
            </div>
            <Switch
              checked={hotspot}
              onCheckedChange={onToggle}
              disabled={busy}
              aria-label={t("settings.network.hotspotTitle")}
            />
          </div>
        </CardContent>
      </Card>

      <WifiPanel />

      <ConfirmDialog
        open={pendingEnable}
        onOpenChange={(open) => {
          if (!open) setPendingEnable(false);
        }}
        title={t("settings.network.confirmTitle")}
        description={
          <Trans
            i18nKey={
              hotspotSsid.known
                ? "settings.network.confirmBodyNamed"
                : "settings.network.confirmBodyUnnamed"
            }
            values={{
              current: wifiStatus.data?.ssid ?? t("settings.network.aWifiNetwork"),
              ssid: hotspotSsid.known ? hotspotSsid.ssid : "",
            }}
            components={{
              current: <span className="font-mono font-medium" />,
              code: <span className="font-mono" />,
            }}
          />
        }
        confirmLabel={t("settings.network.confirmLabel")}
        destructive
        onConfirm={confirmEnable}
      />
    </div>
  );
}
