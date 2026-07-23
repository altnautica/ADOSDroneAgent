import { useQuery } from "@tanstack/react-query";
import { useEffect, useMemo, useState } from "react";

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

const HOTSPOT_NAME_PREFIX = "ADOS-";

export function NetworkSettings() {
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

  const deviceSuffix = useMemo(() => {
    const id = status.data?.device_id ?? "";
    return id ? `${HOTSPOT_NAME_PREFIX}${id}` : `${HOTSPOT_NAME_PREFIX}<device>`;
  }, [status.data?.device_id]);

  async function commitHotspot(next: boolean) {
    setBusy(true);
    try {
      const res = await postApply({ network: { hotspot_enabled: next } });
      const section = res.sections.network;
      if (!res.overall || !section?.ok) {
        setHotspot(!next);
        toast.err(section?.message ?? "Hotspot toggle failed.");
      } else {
        toast.ok(section.message || "Hotspot updated.");
      }
    } catch (err) {
      setHotspot(!next);
      toastFromError(err, "Hotspot toggle failed.");
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
                Setup hotspot
                <RiskBadge tone="auto" />
              </div>
              <div className="text-xs text-muted-foreground leading-relaxed">
                Spins up a captive AP named{" "}
                <span className="font-mono">{deviceSuffix}</span> on
                wlan0 so a phone or laptop can join the agent over Wi-Fi
                and finish onboarding without a wired connection.
              </div>
              <div className="text-xs text-warn">
                Enabling this disconnects any active Wi-Fi client link
                (the radio can host one or the other, not both).
              </div>
            </div>
            <Switch
              checked={hotspot}
              onCheckedChange={onToggle}
              disabled={busy}
              aria-label="Setup hotspot"
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
        title="Switch wlan0 to setup AP?"
        description={
          <>
            The agent is currently joined to{" "}
            <span className="font-mono font-medium">
              {wifiStatus.data?.ssid ?? "a Wi-Fi network"}
            </span>
            . Enabling the setup hotspot will drop that link. The rig
            stays reachable via the{" "}
            <span className="font-mono">{deviceSuffix}</span> AP or any
            other uplink (ethernet, USB tether, 4G) until you toggle
            the hotspot back off.
          </>
        }
        confirmLabel="Switch to AP"
        destructive
        onConfirm={confirmEnable}
      />
    </div>
  );
}
