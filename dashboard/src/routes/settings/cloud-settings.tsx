import { useEffect, useState } from "react";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { useStatus } from "@/hooks/use-status";
import { ApiError } from "@/lib/api";
import { cloudSectionSchema, postApply } from "@/lib/apply-actions";

type CloudMode = "cloud" | "self_hosted" | "local";

const MODE_OPTIONS = [
  {
    value: "cloud" as const,
    label: "Altnautica relay",
    description:
      "Use the public Altnautica cloud relay for telemetry and video.",
  },
  {
    value: "self_hosted" as const,
    label: "Self-hosted",
    description:
      "Point at your own Convex backend and MQTT broker.",
  },
  {
    value: "local" as const,
    label: "Local only",
    description:
      "No cloud uplink. LAN access via /api and direct dashboard only.",
  },
];

export function CloudSettings() {
  const status = useStatus();

  const initialMode = (status.data?.cloud_choice?.mode as CloudMode) ?? "cloud";
  const initialUrl = status.data?.cloud_choice?.backend_url ?? "";
  const initialBroker = status.data?.cloud_choice?.mqtt_broker ?? "";
  const initialPort = status.data?.cloud_choice?.mqtt_port ?? 8883;

  const [mode, setMode] = useState<CloudMode>(initialMode);
  const [url, setUrl] = useState(initialUrl);
  const [broker, setBroker] = useState(initialBroker);
  const [port, setPort] = useState(String(initialPort));
  const [apiKey, setApiKey] = useState("");

  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [feedback, setFeedback] = useState<{
    kind: "ok" | "err";
    text: string;
  } | null>(null);
  const [validationError, setValidationError] = useState<string | null>(null);

  useEffect(() => {
    if (status.data) {
      setMode((status.data.cloud_choice?.mode as CloudMode) ?? "cloud");
      setUrl(status.data.cloud_choice?.backend_url ?? "");
      setBroker(status.data.cloud_choice?.mqtt_broker ?? "");
      setPort(String(status.data.cloud_choice?.mqtt_port ?? 8883));
    }
  }, [status.data]);

  const dirty =
    mode !== initialMode ||
    (mode === "self_hosted" &&
      (url !== initialUrl ||
        broker !== initialBroker ||
        port !== String(initialPort) ||
        apiKey !== ""));

  function validate(): boolean {
    const payload =
      mode === "self_hosted"
        ? {
            mode,
            self_hosted: {
              url,
              mqtt_broker: broker,
              mqtt_port: Number(port),
              api_key: apiKey || undefined,
            },
          }
        : { mode };
    const result = cloudSectionSchema.safeParse(payload);
    if (!result.success) {
      const first = result.error.issues[0];
      setValidationError(`${first.path.join(".")}: ${first.message}`);
      return false;
    }
    setValidationError(null);
    return true;
  }

  async function handleApply() {
    setBusy(true);
    setFeedback(null);
    try {
      const res = await postApply({
        cloud:
          mode === "self_hosted"
            ? {
                mode,
                self_hosted: {
                  url,
                  mqtt_broker: broker,
                  mqtt_port: Number(port),
                  api_key: apiKey || undefined,
                },
              }
            : { mode },
      });
      const section = res.sections.cloud;
      if (res.overall && section?.ok) {
        setFeedback({
          kind: "ok",
          text: section.message || "Cloud posture saved.",
        });
        setApiKey("");
      } else {
        setFeedback({
          kind: "err",
          text: section?.message ?? "Apply failed.",
        });
      }
    } catch (err) {
      setFeedback({
        kind: "err",
        text:
          err instanceof ApiError
            ? `${err.status}: ${err.message}`
            : err instanceof Error
              ? err.message
              : String(err),
      });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <div>
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          Cloud posture
          <RiskBadge tone="manual" />
        </div>
        <RadioCardGroup
          value={mode}
          onChange={(v) => setMode(v as CloudMode)}
          options={MODE_OPTIONS}
          columns={3}
        />
      </div>

      {mode === "self_hosted" && (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-4">
            <div className="flex items-center gap-2 text-sm font-semibold">
              Self-hosted endpoints
              <RiskBadge tone="manual" />
            </div>

            <div className="space-y-2">
              <Label htmlFor="cloud-url">Convex URL</Label>
              <Input
                id="cloud-url"
                type="url"
                placeholder="https://convex.example.com"
                value={url}
                onChange={(e) => {
                  setUrl(e.target.value);
                  setValidationError(null);
                }}
              />
            </div>

            <div className="grid grid-cols-1 sm:grid-cols-[1fr_120px] gap-3">
              <div className="space-y-2">
                <Label htmlFor="cloud-broker">MQTT broker</Label>
                <Input
                  id="cloud-broker"
                  placeholder="mqtt.example.com"
                  value={broker}
                  onChange={(e) => {
                    setBroker(e.target.value);
                    setValidationError(null);
                  }}
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="cloud-port">Port</Label>
                <Input
                  id="cloud-port"
                  type="number"
                  min={1}
                  max={65535}
                  value={port}
                  onChange={(e) => {
                    setPort(e.target.value);
                    setValidationError(null);
                  }}
                />
              </div>
            </div>

            <div className="space-y-2">
              <Label htmlFor="cloud-api-key">API key</Label>
              <Input
                id="cloud-api-key"
                type="password"
                autoComplete="new-password"
                placeholder="leave blank to keep existing"
                value={apiKey}
                onChange={(e) => {
                  setApiKey(e.target.value);
                  setValidationError(null);
                }}
              />
              <p className="text-[11px] text-muted-foreground">
                Write-only. Stored as a root-owned secret on the agent.
              </p>
            </div>

            {validationError && (
              <div className="rounded-md border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs text-red-700 dark:text-red-300">
                {validationError}
              </div>
            )}
          </CardContent>
        </Card>
      )}

      {feedback && (
        <div
          className={`rounded-md border px-3 py-2 text-sm ${
            feedback.kind === "ok"
              ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
              : "border-red-500/40 bg-red-500/10 text-red-700 dark:text-red-300"
          }`}
        >
          {feedback.text}
        </div>
      )}

      <div className="flex items-center justify-end gap-3">
        {dirty && (
          <span className="text-xs text-muted-foreground">unsaved changes</span>
        )}
        <Button
          variant="default"
          disabled={!dirty || busy}
          onClick={() => {
            if (validate()) setConfirmOpen(true);
          }}
        >
          Save cloud
        </Button>
      </div>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title="Switch cloud posture?"
        description={
          <>
            The cloud relay client will reconnect using the new posture (
            <span className="font-mono font-medium">{mode}</span>). Existing
            MQTT and HTTP sessions drop. Pairing is preserved.
          </>
        }
        confirmLabel="Apply"
        destructive
        onConfirm={handleApply}
      />
    </div>
  );
}
