import { Bluetooth, Gamepad2, Monitor, RefreshCw } from "lucide-react";
import { useState } from "react";

import { PageShell } from "@/components/page-shell";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/lib/api";
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
      setScanError(e instanceof Error ? e.message : "scan failed");
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
        toast.err("Pairing failed.", res.error ?? undefined);
      } else if (res.connected === false) {
        toast.err("Paired, but the device did not connect.", res.error ?? undefined);
      } else {
        toast.ok("Paired and connected.");
      }
      await bluetooth.refetch();
    } catch (err) {
      toastFromError(err, "Pairing failed.");
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
      toastFromError(err, "Could not set the primary controller.");
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
      title="Display & Joystick"
      blurb="HDMI kiosk display, joystick / gamepad assignment, and Bluetooth pairings for this ground station."
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
          <RefreshCw className="h-3.5 w-3.5" /> Refresh
        </Button>
      }
    >
      <div className="grid gap-4 md:grid-cols-2">
        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Monitor className="h-3.5 w-3.5" />
              Kiosk display
            </CardTitle>
          </CardHeader>
          <CardContent>
            {display.isError ? (
              <p className="text-sm text-destructive">Could not read the display config.</p>
            ) : !disp ? (
              <p className="text-sm text-muted-foreground">loading…</p>
            ) : (
              <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
                <div className="text-xs text-muted-foreground">kiosk</div>
                <div className="font-mono">{disp.kiosk_enabled ? "on" : "off"}</div>
                <div className="text-xs text-muted-foreground">resolution</div>
                <div className="font-mono">{disp.resolution ?? "—"}</div>
                <div className="text-xs text-muted-foreground">target</div>
                <div className="font-mono truncate" title={disp.kiosk_target_url ?? undefined}>
                  {disp.kiosk_target_url ?? "default"}
                </div>
              </div>
            )}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <Gamepad2 className="h-3.5 w-3.5" />
              Gamepads / joysticks
            </CardTitle>
          </CardHeader>
          <CardContent>
            {gamepads.isError ? (
              <p className="text-sm text-destructive">Could not read the attached controllers.</p>
            ) : pads.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                No game controllers attached. Plug in a USB joystick or pair a
                Bluetooth controller below.
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
                          {pad.name || "Unnamed controller"}
                        </div>
                        <div className="text-xs text-muted-foreground font-mono truncate">
                          {pad.path ?? pad.device_id}
                        </div>
                      </div>
                      {isPrimary ? (
                        <Badge variant="ok">primary</Badge>
                      ) : (
                        <Button
                          variant="outline"
                          size="sm"
                          disabled={primaryBusy !== null}
                          onClick={() => void makePrimary(pad.device_id)}
                        >
                          {primaryBusy === pad.device_id ? "Setting…" : "Make primary"}
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
                Bluetooth
              </span>
              <Button
                variant="outline"
                size="sm"
                onClick={() => void scanBluetooth()}
                disabled={scanning}
              >
                {scanning ? `Scanning ${BT_SCAN_SECONDS}s…` : "Scan"}
              </Button>
            </CardTitle>
          </CardHeader>
          <CardContent className="space-y-4">
            <div>
              <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1.5">
                Paired
              </div>
              {paired.length === 0 ? (
                <p className="text-sm text-muted-foreground">
                  No paired Bluetooth devices. Scan to find a controller nearby.
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
                        {d.connected ? "connected" : "paired"}
                      </Badge>
                    </li>
                  ))}
                </ul>
              )}
            </div>

            {discovered !== null && (
              <div>
                <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1.5">
                  Found nearby
                </div>
                {unpairedFound.length === 0 ? (
                  <p className="text-sm text-muted-foreground">
                    No unpaired devices found. Put the controller in pairing mode
                    and scan again.
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
                          {pairingMac === d.mac ? "Pairing…" : "Pair"}
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
