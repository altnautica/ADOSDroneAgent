import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Loader2, RefreshCw, Wifi, WifiOff } from "lucide-react";
import { useMemo, useState } from "react";

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
      toast.ok("Disconnected.");
      refreshAll();
    },
    onError: (err) => toastFromError(err, "Disconnect failed."),
  });

  const forgetMutation = useMutation({
    mutationFn: (name: string) => forgetWifi(name),
    onSuccess: () => {
      toast.ok("Network forgotten.");
      refreshAll();
    },
    onError: (err) => toastFromError(err, "Forget failed."),
  });

  const autoconnectMutation = useMutation({
    mutationFn: ({ name, enabled }: { name: string; enabled: boolean }) =>
      setWifiAutoconnect(name, enabled),
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: SAVED_KEY });
    },
    onError: (err) => toastFromError(err, "Auto-connect toggle failed."),
  });

  const savedByName = useMemo(() => {
    const map = new Map<string, WifiSavedConnection>();
    for (const c of saved.data?.connections ?? []) {
      map.set(c.name, c);
    }
    return map;
  }, [saved.data]);

  const networks = useMemo(() => scan.data?.networks ?? [], [scan.data]);
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
                Wi-Fi client
                {status.isLoading ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin text-muted-foreground" />
                ) : isConnected ? (
                  <Badge variant="ok" className="font-normal">
                    <Wifi className="h-3 w-3 mr-1" />
                    connected
                  </Badge>
                ) : (
                  <Badge variant="default" className="font-normal">
                    <WifiOff className="h-3 w-3 mr-1" />
                    not connected
                  </Badge>
                )}
              </div>
              <p className="text-xs text-muted-foreground">
                {isConnected && currentStatus?.ssid
                  ? `Connected to "${currentStatus.ssid}"${
                      currentStatus.ip ? ` · ${currentStatus.ip}` : ""
                    }${
                      currentStatus.signal != null
                        ? ` · ${currentStatus.signal}%`
                        : ""
                    }`
                  : "Pick a nearby network to join, or add a hidden SSID."}
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
                  Disconnect
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
                {scanning ? "Scanning…" : "Refresh"}
              </Button>
            </div>
          </div>

          {/* Saved networks */}
          {savedNetworks.length > 0 && (
            <div className="space-y-2">
              <div className="text-xs uppercase tracking-wider text-muted-foreground">
                Saved networks
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
            <div className="text-xs uppercase tracking-wider text-muted-foreground">
              Nearby networks
            </div>
            {networks.length === 0 && !scanning && (
              <div className="rounded-md border border-dashed border-border px-3 py-6 text-center text-xs text-muted-foreground">
                No networks found. Tap Refresh to scan again.
              </div>
            )}
            {networks.length > 0 && (
              <div className="rounded-md border border-border divide-y divide-border">
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
        title="Forget this network?"
        description={
          forgetTarget ? (
            <>
              Remove the saved credentials for{" "}
              <span className="font-mono font-medium">{forgetTarget.name}</span>.
              The agent will not auto-reconnect until you join it again.
            </>
          ) : null
        }
        confirmLabel="Forget"
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
