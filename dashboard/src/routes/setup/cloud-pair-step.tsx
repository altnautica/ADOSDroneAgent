import { useEffect, useState } from "react";

import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { usePairingInfo } from "@/hooks/use-pairing";
import { useStatus } from "@/hooks/use-status";

type CloudMode = "cloud" | "self_hosted" | "local";

interface Props {
  onChange: (state: {
    mode: CloudMode;
    backend_url?: string;
    mqtt_broker?: string;
    mqtt_port?: number;
    api_key?: string;
    isValid: boolean;
  }) => void;
}

const MODE_OPTIONS: ReadonlyArray<{
  value: CloudMode;
  label: string;
  description: string;
}> = [
  {
    value: "cloud",
    label: "Altnautica cloud",
    description: "Hosted MQTT relay + Convex backend. Easiest path; works out of the box.",
  },
  {
    value: "self_hosted",
    label: "Self-hosted",
    description:
      "Point the agent at your own Convex deployment + MQTT broker. Best for fleet operators.",
  },
  {
    value: "local",
    label: "Local only",
    description: "No cloud. Pair on the LAN with Mission Control directly. Lowest dependency.",
  },
];

export function CloudPairStep({ onChange }: Props) {
  const status = useStatus();
  const pairing = usePairingInfo();

  const initial = (status.data?.cloud_choice?.mode as CloudMode) ?? "cloud";
  const [mode, setMode] = useState<CloudMode>(initial);
  const [backendUrl, setBackendUrl] = useState(
    status.data?.cloud_choice?.backend_url ?? "",
  );
  const [mqttBroker, setMqttBroker] = useState(
    status.data?.cloud_choice?.mqtt_broker ?? "",
  );
  const [mqttPort, setMqttPort] = useState<string>(
    String(status.data?.cloud_choice?.mqtt_port ?? 8883),
  );
  const [apiKey, setApiKey] = useState("");

  useEffect(() => {
    if (mode === "self_hosted") {
      const portNum = Number(mqttPort);
      const valid =
        backendUrl.trim().length > 0 &&
        mqttBroker.trim().length > 0 &&
        Number.isFinite(portNum) &&
        portNum > 0;
      onChange({
        mode,
        backend_url: backendUrl.trim(),
        mqtt_broker: mqttBroker.trim(),
        mqtt_port: portNum,
        api_key: apiKey.trim() || undefined,
        isValid: valid,
      });
      return;
    }
    onChange({ mode, isValid: true });
  }, [mode, backendUrl, mqttBroker, mqttPort, apiKey, onChange]);

  const code = pairing.data?.pairing_code ?? "";
  const paired = pairing.data?.paired ?? false;
  const pairedCount = pairing.data?.paired_with?.length ?? 0;

  return (
    <div className="space-y-6">
      <div>
        <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          Cloud posture
        </div>
        <RadioCardGroup
          value={mode}
          onChange={(v) => setMode(v)}
          options={MODE_OPTIONS}
          columns={3}
        />
      </div>

      {mode === "self_hosted" && (
        <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
          <div className="space-y-1.5 md:col-span-2">
            <Label htmlFor="backend-url">Convex backend URL</Label>
            <Input
              id="backend-url"
              type="url"
              placeholder="https://convex.example.com"
              value={backendUrl}
              onChange={(e) => setBackendUrl(e.target.value)}
            />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="mqtt-broker">MQTT broker</Label>
            <Input
              id="mqtt-broker"
              type="text"
              placeholder="mqtt.example.com"
              value={mqttBroker}
              onChange={(e) => setMqttBroker(e.target.value)}
            />
          </div>
          <div className="space-y-1.5">
            <Label htmlFor="mqtt-port">MQTT port</Label>
            <Input
              id="mqtt-port"
              type="number"
              min={1}
              max={65535}
              value={mqttPort}
              onChange={(e) => setMqttPort(e.target.value)}
            />
          </div>
          <div className="space-y-1.5 md:col-span-2">
            <Label htmlFor="api-key">API key (optional)</Label>
            <Input
              id="api-key"
              type="password"
              placeholder="leave blank if not using auth"
              value={apiKey}
              onChange={(e) => setApiKey(e.target.value)}
            />
          </div>
        </div>
      )}

      <Card>
        <CardContent className="pt-4 space-y-3">
          <div className="flex items-center justify-between">
            <span className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
              Pair with Mission Control
            </span>
            {paired ? (
              <Badge variant="ok">paired ({pairedCount})</Badge>
            ) : (
              <Badge variant="info">unpaired</Badge>
            )}
          </div>
          {paired ? (
            <p className="text-sm text-muted-foreground">
              This drone is already paired with Mission Control. You can manage paired
              devices in <span className="font-mono">/pairing</span>.
            </p>
          ) : code ? (
            <>
              <p className="text-sm text-muted-foreground">
                Open Mission Control and paste this code, or accept a code from
                Mission Control on the Pairing page.
              </p>
              <div className="font-mono text-3xl tracking-[0.4em] py-1">
                {code}
              </div>
              <p className="text-xs text-muted-foreground">
                Code rotates automatically when the agent restarts or after
                pairing succeeds.
              </p>
            </>
          ) : (
            <p className="text-sm text-muted-foreground">
              Awaiting code from the agent…
            </p>
          )}
        </CardContent>
      </Card>
    </div>
  );
}
