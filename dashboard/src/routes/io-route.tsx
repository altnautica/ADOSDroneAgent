import { Bluetooth, Gamepad2, Monitor, RefreshCw } from "lucide-react";
import { useState } from "react";

import { PageShell } from "@/components/page-shell";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/lib/api";

interface GamepadEntry {
  index?: number;
  name?: string;
  id?: string;
  path?: string;
  primary?: boolean;
  connected?: boolean;
}

interface GamepadsResponse {
  gamepads?: GamepadEntry[];
  primary?: string | null;
}

interface DisplayResponse {
  installed?: boolean;
  panel?: string | null;
  resolution?: string | null;
  rotation?: number | null;
  touch?: boolean;
  brightness?: number | null;
}

interface BluetoothDevice {
  mac: string;
  name?: string;
  paired?: boolean;
  connected?: boolean;
  type?: string;
}

interface BluetoothPairedResponse {
  devices?: BluetoothDevice[];
}

export function IoRoute() {
  const [busy, setBusy] = useState(false);
  const [scanError, setScanError] = useState<string | null>(null);

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
  const bluetooth = useResource<BluetoothPairedResponse>(
    "io-bluetooth-paired",
    "/api/v1/ground-station/bluetooth/paired",
    15_000,
  );

  const triggerBtScan = async () => {
    setBusy(true);
    setScanError(null);
    try {
      await apiFetch("/api/v1/ground-station/bluetooth/scan", {
        method: "POST",
      });
      await bluetooth.refetch();
    } catch (e) {
      setScanError(e instanceof Error ? e.message : "scan failed");
    } finally {
      setBusy(false);
    }
  };

  const padList = gamepads.data?.gamepads ?? [];
  const btDevices = bluetooth.data?.devices ?? [];
  const disp = display.data;

  return (
    <PageShell
      title="Display & Joystick"
      blurb="HDMI panel state, joystick / gamepad assignment, and Bluetooth pairings for this ground station."
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
              Display
            </CardTitle>
          </CardHeader>
          <CardContent>
            {!disp || disp.installed === false ? (
              <p className="text-sm text-muted-foreground">
                No display panel configured. Run{" "}
                <code className="text-xs">ados display install</code> to install
                a panel driver.
              </p>
            ) : (
              <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-sm">
                <div className="text-xs text-muted-foreground">panel</div>
                <div className="font-mono">{disp.panel ?? "—"}</div>
                <div className="text-xs text-muted-foreground">resolution</div>
                <div className="font-mono">{disp.resolution ?? "—"}</div>
                <div className="text-xs text-muted-foreground">rotation</div>
                <div className="font-mono">
                  {disp.rotation != null ? `${disp.rotation}°` : "—"}
                </div>
                <div className="text-xs text-muted-foreground">touch</div>
                <div className="font-mono">{disp.touch ? "yes" : "no"}</div>
                <div className="text-xs text-muted-foreground">brightness</div>
                <div className="font-mono">{disp.brightness ?? "—"}</div>
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
            {padList.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                No game controllers attached. Plug in a USB joystick or pair a
                Bluetooth controller below.
              </p>
            ) : (
              <ul className="space-y-2 text-sm">
                {padList.map((pad, i) => (
                  <li
                    key={pad.id ?? pad.path ?? i}
                    className="flex items-center justify-between gap-2 border-b border-border/40 pb-2 last:border-b-0 last:pb-0"
                  >
                    <div className="min-w-0">
                      <div className="font-medium truncate">
                        {pad.name ?? "Unknown controller"}
                      </div>
                      <div className="text-xs text-muted-foreground font-mono truncate">
                        {pad.path ?? pad.id ?? "—"}
                      </div>
                    </div>
                    {pad.primary ? (
                      <Badge variant="ok">primary</Badge>
                    ) : pad.connected ? (
                      <Badge variant="outline">connected</Badge>
                    ) : (
                      <Badge variant="outline">idle</Badge>
                    )}
                  </li>
                ))}
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
                onClick={triggerBtScan}
                disabled={busy}
              >
                Scan
              </Button>
            </CardTitle>
          </CardHeader>
          <CardContent>
            {btDevices.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                No paired Bluetooth devices. Press Scan to discover nearby
                controllers, then pair via the agent CLI.
              </p>
            ) : (
              <ul className="space-y-2 text-sm">
                {btDevices.map((d) => (
                  <li
                    key={d.mac}
                    className="flex items-center justify-between gap-2 border-b border-border/40 pb-2 last:border-b-0 last:pb-0"
                  >
                    <div className="min-w-0">
                      <div className="font-medium truncate">
                        {d.name ?? d.mac}
                      </div>
                      <div className="text-xs text-muted-foreground font-mono truncate">
                        {d.mac}
                        {d.type ? ` · ${d.type}` : ""}
                      </div>
                    </div>
                    {d.connected ? (
                      <Badge variant="ok">connected</Badge>
                    ) : d.paired ? (
                      <Badge variant="outline">paired</Badge>
                    ) : (
                      <Badge variant="outline">seen</Badge>
                    )}
                  </li>
                ))}
              </ul>
            )}
            {scanError && (
              <p className="text-xs text-destructive mt-2">{scanError}</p>
            )}
          </CardContent>
        </Card>
      </div>
    </PageShell>
  );
}
