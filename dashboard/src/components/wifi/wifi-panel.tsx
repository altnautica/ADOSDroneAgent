import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Loader2, RefreshCw, Wifi, WifiOff } from "lucide-react";
import { useMemo, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { toast, toastFromError } from "@/lib/toast";
import {
  forgetWifi,
  getSavedWifi,
  getWifiStatus,
  leaveWifi,
  scanWifi,
  setWifiAutoconnect,
} from "@/lib/wifi";
import type {
  WifiNetwork,
  WifiSavedConnection,
} from "@/lib/types";

import { WifiManualEntryRow } from "./wifi-manual-entry-row";
import { WifiNetworkRow } from "./wifi-network-row";
import { WifiPasswordModal } from "./wifi-password-modal";
import { WifiSavedRow } from "./wifi-saved-row";

const STATUS_KEY = ["wifi", "status"] as const;
const SCAN_KEY = ["wifi", "scan"] as const;
const SAVED_KEY = ["wifi", "saved"] as const;

export function WifiPanel() {
  const { t } = useTranslation();
  const qc = useQueryClient();

  const status = useQuery({
    queryKey: STATUS_KEY,
    queryFn: () => getWifiStatus(),
    refetchInterval: 5_000,
  });

  const scan = useQuery({
    queryKey: SCAN_KEY,
    queryFn: () => scanWifi(),
    // Scans block ~10s on nmcli; only refetch on explicit refresh.
    refetchInterval: false,
    refetchOnWindowFocus: false,
    staleTime: 30_000,
  });

  const saved = useQuery({
    queryKey: SAVED_KEY,
    queryFn: () => getSavedWifi(),
    refetchInterval: 15_000,
  });

  const [selected, setSelected] = useState<{
    ssid: string;
    security: string;
    saved: boolean;
  } | null>(null);
  const [forgetTarget, setForgetTarget] = useState<WifiSavedConnection | null>(null);

  const refreshAll = () => {
    void qc.invalidateQueries({ queryKey: STATUS_KEY });
    void qc.invalidateQueries({ queryKey: SCAN_KEY });
    void qc.invalidateQueries({ queryKey: SAVED_KEY });
  };

  const leaveMutation = useMutation({
    mutationFn: leaveWifi,
    onSuccess: () => {
      toast.ok(t("wifi.panel.disconnected"));
      refreshAll();
    },
    onError: (err) => toastFromError(err, t("wifi.panel.disconnectFailed")),
  });

  const forgetMutation = useMutation({
    mutationFn: (name: string) => forgetWifi(name),
    onSuccess: () => {
      toast.ok(t("wifi.panel.forgotten"));
      refreshAll();
    },
    onError: (err) => toastFromError(err, t("wifi.panel.forgetFailed")),
  });

  const autoconnectMutation = useMutation({
    mutationFn: ({ name, enabled }: { name: string; enabled: boolean }) =>
      setWifiAutoconnect(name, enabled),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: SAVED_KEY });
    },
    onError: (err) => toastFromError(err, t("wifi.panel.autoconnectFailed")),
  });

  const savedByName = useMemo(() => {
    const map = new Map<string, WifiSavedConnection>();
    for (const c of saved.data?.connections ?? []) {
      map.set(c.name, c);
    }
    return map;
  }, [saved.data]);

  const networks = useMemo(() => {
    // Collapse same-SSID rows that nmcli emits one per BSSID. Keep the
    // strongest signal as the surviving row, and inherit `in_use` if
    // any of the duplicates is the currently-connected BSSID. Hidden
    // networks (empty SSID) pass through unchanged because they can't
    // be deduplicated by name.
    const all = scan.data?.networks ?? [];
    const grouped = new Map<string, WifiNetwork>();
    const hidden: WifiNetwork[] = [];
    for (const net of all) {
      if (!net.ssid) {
        hidden.push(net);
        continue;
      }
      const prior = grouped.get(net.ssid);
      if (!prior) {
        grouped.set(net.ssid, net);
        continue;
      }
      const strongerSignal = net.signal > prior.signal ? net : prior;
      grouped.set(net.ssid, {
        ...strongerSignal,
        in_use: prior.in_use || net.in_use,
      });
    }
    const merged = [...grouped.values(), ...hidden];
    merged.sort((a, b) => b.signal - a.signal);
    return merged;
  }, [scan.data]);
  const savedNetworks = saved.data?.connections ?? [];
  const currentStatus = status.data ?? null;
  const isConnected = !!currentStatus?.connected;
  const scanning = scan.isFetching;

  function openPasswordFor(net: WifiNetwork) {
    setSelected({
      ssid: net.ssid,
      security: net.security,
      saved: savedByName.has(net.ssid),
    });
  }

  function openManual(ssid: string) {
    if (!ssid.trim()) return;
    setSelected({
      ssid: ssid.trim(),
      security: "WPA2",
      saved: savedByName.has(ssid.trim()),
    });
  }

  return (
    <>
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          {/* Header + connected banner */}
          <div className="flex items-start justify-between gap-3">
            <div className="space-y-1 min-w-0">
              <div className="flex items-center gap-2 text-sm font-semibold">
                {t("wifi.panel.title")}
                {status.isLoading ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />
                ) : isConnected ? (
                  <Badge variant="ok" className="font-normal">
                    <Wifi className="h-3 w-3 mr-1" />
                    {t("wifi.network.connected")}
                  </Badge>
                ) : (
                  <Badge variant="default" className="font-normal">
                    <WifiOff className="h-3 w-3 mr-1" />
                    {t("wifi.panel.notConnected")}
                  </Badge>
                )}
              </div>
              <p className="text-xs text-muted-foreground">
                {isConnected && currentStatus?.ssid
                  ? `${t("wifi.panel.connectedTo", { ssid: currentStatus.ssid })}${
                      currentStatus.ip ? ` · ${currentStatus.ip}` : ""
                    }${
                      currentStatus.signal != null
                        ? ` · ${currentStatus.signal}%`
                        : ""
                    }`
                  : t("wifi.panel.pickHint")}
              </p>
            </div>
            <div className="flex items-center gap-2 shrink-0">
              {isConnected && (
                <Button
                  variant="outline"
                  size="sm"
                  disabled={leaveMutation.isPending}
                  onClick={() => leaveMutation.mutate()}
                >
                  {t("wifi.panel.disconnect")}
                </Button>
              )}
              <Button
                variant="outline"
                size="sm"
                disabled={scanning}
                onClick={() =>
                  void qc.invalidateQueries({ queryKey: SCAN_KEY })
                }
              >
                <RefreshCw
                  className={`h-3.5 w-3.5 mr-1.5 ${
                    scanning ? "animate-spin" : ""
                  }`}
                />
                {scanning ? t("wifi.panel.scanning") : t("wifi.panel.refresh")}
              </Button>
            </div>
          </div>

          {/* Saved networks */}
          {savedNetworks.length > 0 && (
            <div className="space-y-2">
              <div className="text-xs uppercase tracking-wider text-muted-foreground">
                {t("wifi.panel.savedNetworks")}
              </div>
              <div className="rounded-md border border-border divide-y divide-border">
                {savedNetworks.map((c) => (
                  <WifiSavedRow
                    key={c.name}
                    connection={c}
                    currentSsid={currentStatus?.ssid ?? null}
                    autoconnectBusy={autoconnectMutation.isPending}
                    onAutoconnect={(enabled) =>
                      autoconnectMutation.mutate({ name: c.name, enabled })
                    }
                    onForget={() => setForgetTarget(c)}
                  />
                ))}
              </div>
            </div>
          )}

          {/* Nearby networks */}
          <div className="space-y-2">
            <div className="flex items-center justify-between gap-2">
              <div className="text-xs uppercase tracking-wider text-muted-foreground">
                {t("wifi.panel.nearbyNetworks")}
              </div>
              {networks.length > 0 && (
                <div className="text-[11px] text-muted-foreground font-mono">
                  {t("wifi.panel.found", { count: networks.length })}
                </div>
              )}
            </div>
            {networks.length === 0 && !scanning && (
              <div className="rounded-md border border-dashed border-border px-3 py-6 text-center text-xs text-muted-foreground">
                {t("wifi.panel.noNetworks")}
              </div>
            )}
            {networks.length > 0 && (
              <div className="rounded-md border border-border max-h-96 overflow-y-auto divide-y divide-border">
                {networks.map((n) => (
                  <WifiNetworkRow
                    key={`${n.ssid}-${n.bssid}`}
                    network={n}
                    isCurrent={n.in_use || n.ssid === currentStatus?.ssid}
                    isSaved={savedByName.has(n.ssid)}
                    onSelect={() => openPasswordFor(n)}
                  />
                ))}
              </div>
            )}
            <WifiManualEntryRow onSubmit={openManual} />
          </div>
        </CardContent>
      </Card>

      <WifiPasswordModal
        target={selected}
        onClose={() => setSelected(null)}
        onJoined={refreshAll}
      />

      <ConfirmDialog
        open={forgetTarget !== null}
        onOpenChange={(open) => {
          if (!open) setForgetTarget(null);
        }}
        title={t("wifi.panel.forgetTitle")}
        description={
          forgetTarget ? (
            <Trans
              i18nKey="wifi.panel.forgetDescription"
              values={{ name: forgetTarget.name }}
              components={{ name: <span className="font-mono font-medium" /> }}
            />
          ) : null
        }
        confirmLabel={t("wifi.saved.forget")}
        destructive
        onConfirm={async () => {
          if (!forgetTarget) return;
          await forgetMutation.mutateAsync(forgetTarget.name);
          setForgetTarget(null);
        }}
      />
    </>
  );
}
