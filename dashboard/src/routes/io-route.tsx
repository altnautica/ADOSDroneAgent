import { Bluetooth, Gamepad2, Monitor, RefreshCw } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";

import { PageShell } from "@/components/page-shell";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/shared/api-fetch";
import { toast, toastFromError } from "@/lib/toast";

// GET /api/v1/ground-station/gamepads — the live evdev controllers plus the
// persisted primary selection.
interface GamepadDevice {
  device_id: string;
  name?: string | null;
  path?: string | null;
  connected?: boolean;
}

interface GamepadsResponse {
  devices: GamepadDevice[];
  primary_id: string | null;
}

// GET /api/v1/ground-station/display — the persisted HDMI kiosk config.
interface DisplayResponse {
  resolution: string | null;
  kiosk_enabled: boolean;
  kiosk_target_url: string | null;
}

// GET /api/v1/ground-station/bluetooth/paired
interface PairedBluetoothDevice {
  mac: string;
  name?: string | null;
  type?: string | null;
  connected?: boolean;
}

// POST /api/v1/ground-station/bluetooth/scan
interface DiscoveredBluetoothDevice {
  mac: string;
  name?: string | null;
}

interface BluetoothPairResult {
  paired: boolean;
  connected?: boolean;
  error: string | null;
}

/** How long a Bluetooth discovery scan listens, in seconds. */
const BT_SCAN_SECONDS = 8;

export function IoRoute() {
  const { t } = useTranslation();
  const [scanning, setScanning] = useState(false);
  const [scanError, setScanError] = useState<string | null>(null);
  const [discovered, setDiscovered] = useState<DiscoveredBluetoothDevice[] | null>(null);
  const [pairingMac, setPairingMac] = useState<string | null>(null);
  const [primaryBusy, setPrimaryBusy] = useState<string | null>(null);

  const gamepads = useResource<GamepadsResponse>(
    "io-gamepads",
    "/api/v1/ground-station/gamepads",
    5_000,
  );
  const display = useResource<DisplayResponse>(
    "io-display",
    "/api/v1/ground-station/display",
    10_000,
  );
  const bluetooth = useResource<{ devices: PairedBluetoothDevice[] }>(
    "io-bluetooth-paired",
    "/api/v1/ground-station/bluetooth/paired",
    15_000,
  );

  const scanBluetooth = async () => {
    setScanning(true);
    setScanError(null);
    try {
      const res = await apiFetch<{ devices: DiscoveredBluetoothDevice[] }>(
        "/api/v1/ground-station/bluetooth/scan",
        { method: "POST", body: { duration_s: BT_SCAN_SECONDS } },
      );
      setDiscovered(res.devices ?? []);
    } catch (e) {
      setScanError(e instanceof Error ? e.message : t("io.bluetooth.scanFailed"));
    } finally {
      setScanning(false);
    }
  };

  const pairBluetooth = async (mac: string) => {
    setPairingMac(mac);
    try {
      const res = await apiFetch<BluetoothPairResult>(
        "/api/v1/ground-station/bluetooth/pair",
        { method: "POST", body: { mac } },
      );
      if (!res.paired) {
        toast.err(t("io.bluetooth.pairingFailed"), res.error ?? undefined);
      } else if (res.connected === false) {
        toast.err(t("io.bluetooth.pairedNotConnected"), res.error ?? undefined);
      } else {
        toast.ok(t("io.bluetooth.pairedConnected"));
      }
      await bluetooth.refetch();
    } catch (err) {
      toastFromError(err, t("io.bluetooth.pairingFailed"));
    } finally {
      setPairingMac(null);
    }
  };

  const makePrimary = async (deviceId: string) => {
    setPrimaryBusy(deviceId);
    try {
      await apiFetch("/api/v1/ground-station/gamepads/primary", {
        method: "PUT",
        body: { device_id: deviceId },
      });
      await gamepads.refetch();
    } catch (err) {
      toastFromError(err, t("io.gamepads.setPrimaryFailed"));
    } finally {
      setPrimaryBusy(null);
    }
  };

  const pads = gamepads.data?.devices ?? [];
  const primaryId = gamepads.data?.primary_id ?? null;
  const paired = bluetooth.data?.devices ?? [];
  const pairedMacs = new Set(paired.map((d) => d.mac.toUpperCase()));
  const unpairedFound = (discovered ?? []).filter(
    (d) => !pairedMacs.has(d.mac.toUpperCase()),
  );
  const disp = display.data;

  return (
    <PageShell
      title={t("io.route.title")}
      blurb={t("io.route.blurb")}
      rightAction={
        <Button
          variant="outline"
          size="sm"
          onClick={() => {
            gamepads.refetch();
            display.refetch();
            bluetooth.refetch();
          }}
        >
          <RefreshCw className="h-3.5 w-3.5" /> {t("io.route.refresh")}
        </Button>
      }
    >
      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Monitor className="h-3.5 w-3.5" />
              {t("io.display.title")}
            </CardTitle>
          </CardHeader>
          <CardContent>
            {display.isError ? (
              <p className="text-sm text-destructive">{t("io.display.readFailed")}</p>
            ) : !disp ? (
              <p className="text-sm text-muted-foreground">{t("io.display.loading")}</p>
            ) : (
              <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
                <div className="text-xs text-muted-foreground">{t("io.display.kiosk")}</div>
                <div className="font-mono">{disp.kiosk_enabled ? t("io.display.on") : t("io.display.off")}</div>
                <div className="text-xs text-muted-foreground">{t("io.display.resolution")}</div>
                <div className="font-mono">{disp.resolution ?? "—"}</div>
                <div className="text-xs text-muted-foreground">{t("io.display.target")}</div>
                <div className="font-mono truncate" title={disp.kiosk_target_url ?? undefined}>
                  {disp.kiosk_target_url ?? t("io.display.default")}
                </div>
              </div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Gamepad2 className="h-3.5 w-3.5" />
              {t("io.gamepads.title")}
            </CardTitle>
          </CardHeader>
          <CardContent>
            {gamepads.isError ? (
              <p className="text-sm text-destructive">{t("io.gamepads.readFailed")}</p>
            ) : pads.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                {t("io.gamepads.empty")}
              </p>
            ) : (
              <ul className="space-y-2 text-sm">
                {pads.map((pad) => {
                  const isPrimary = pad.device_id === primaryId;
                  return (
                    <li
                      key={pad.device_id}
                      className="flex items-center justify-between gap-2 border-b border-border/40 pb-2 last:border-b-0 last:pb-0"
                    >
                      <div className="min-w-0">
                        <div className="font-medium truncate">
                          {pad.name || t("io.gamepads.unnamed")}
                        </div>
                        <div className="text-xs text-muted-foreground font-mono truncate">
                          {pad.path ?? pad.device_id}
                        </div>
                      </div>
                      {isPrimary ? (
                        <Badge variant="ok">{t("io.gamepads.primary")}</Badge>
                      ) : (
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={primaryBusy !== null}
                          onClick={() => void makePrimary(pad.device_id)}
                        >
                          {primaryBusy === pad.device_id ? t("io.gamepads.setting") : t("io.gamepads.makePrimary")}
                        </Button>
                      )}
                    </li>
                  );
                })}
              </ul>
            )}
          </CardContent>
        </Card>

        <Card className="md:col-span-2">
          <CardHeader>
            <CardTitle className="flex items-center justify-between gap-2">
              <span className="flex items-center gap-2">
                <Bluetooth className="h-3.5 w-3.5" />
                {t("io.bluetooth.title")}
              </span>
              <Button
                variant="outline"
                size="sm"
                onClick={() => void scanBluetooth()}
                disabled={scanning}
              >
                {scanning ? t("io.bluetooth.scanning", { seconds: BT_SCAN_SECONDS }) : t("io.bluetooth.scan")}
              </Button>
            </CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <div>
              <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1.5">
                {t("io.bluetooth.pairedHeading")}
              </div>
              {paired.length === 0 ? (
                <p className="text-sm text-muted-foreground">
                  {t("io.bluetooth.noPaired")}
                </p>
              ) : (
                <ul className="space-y-2 text-sm">
                  {paired.map((d) => (
                    <li
                      key={d.mac}
                      className="flex items-center justify-between gap-2 border-b border-border/40 pb-2 last:border-b-0 last:pb-0"
                    >
                      <div className="min-w-0">
                        <div className="font-medium truncate">{d.name || d.mac}</div>
                        <div className="text-xs text-muted-foreground font-mono truncate">
                          {d.mac}
                          {d.type ? ` · ${d.type}` : ""}
                        </div>
                      </div>
                      <Badge variant={d.connected ? "ok" : "outline"}>
                        {d.connected ? t("io.bluetooth.connected") : t("io.bluetooth.paired")}
                      </Badge>
                    </li>
                  ))}
                </ul>
              )}
            </div>

            {discovered !== null && (
              <div>
                <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1.5">
                  {t("io.bluetooth.foundNearby")}
                </div>
                {unpairedFound.length === 0 ? (
                  <p className="text-sm text-muted-foreground">
                    {t("io.bluetooth.noUnpaired")}
                  </p>
                ) : (
                  <ul className="space-y-2 text-sm">
                    {unpairedFound.map((d) => (
                      <li
                        key={d.mac}
                        className="flex items-center justify-between gap-2 border-b border-border/40 pb-2 last:border-b-0 last:pb-0"
                      >
                        <div className="min-w-0">
                          <div className="font-medium truncate">{d.name || d.mac}</div>
                          <div className="text-xs text-muted-foreground font-mono truncate">
                            {d.mac}
                          </div>
                        </div>
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={pairingMac !== null}
                          onClick={() => void pairBluetooth(d.mac)}
                        >
                          {pairingMac === d.mac ? t("io.bluetooth.pairing") : t("io.bluetooth.pair")}
                        </Button>
                      </li>
                    ))}
                  </ul>
                )}
              </div>
            )}

            {scanError && <p className="text-xs text-destructive">{scanError}</p>}
          </CardContent>
        </Card>
      </div>
    </PageShell>
  );
}
