import { useEffect, useMemo, useState } from "react";
import { Trans, useTranslation } from "react-i18next";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { CloudflareTokenCard } from "@/components/settings/cloudflare-token-card";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { useStatus } from "@/hooks/use-status";
import { toast, toastFromError } from "@/lib/toast";
import { cloudSectionSchema, postApply } from "@/lib/apply-actions";

type CloudMode = "cloud" | "self_hosted" | "local";

const MODE_OPTIONS = [
  {
    value: "local" as const,
    labelKey: "settingsMore.cloud.modeLocal",
    descriptionKey: "settingsMore.cloud.modeLocalDesc",
  },
  {
    value: "cloud" as const,
    labelKey: "settingsMore.cloud.modeCloud",
    descriptionKey: "settingsMore.cloud.modeCloudDesc",
  },
  {
    value: "self_hosted" as const,
    labelKey: "settingsMore.cloud.modeSelfHosted",
    descriptionKey: "settingsMore.cloud.modeSelfHostedDesc",
  },
] as const;

// Example hostnames, not translatable copy.
const URL_PLACEHOLDER = "https://convex.example.com";
const BROKER_PLACEHOLDER = "mqtt.example.com";

export function CloudSettings() {
  const { t } = useTranslation();
  const status = useStatus();
  const modeOptions = useMemo(
    () =>
      MODE_OPTIONS.map((o) => ({
        value: o.value,
        label: t(o.labelKey),
        description: t(o.descriptionKey),
      })),
    [t],
  );

  const initialMode = (status.data?.cloud_choice?.mode as CloudMode) ?? "local";
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
  const [validationError, setValidationError] = useState<string | null>(null);

  // Re-seed only when the agent's values change, never on an unrelated poll.
  useEffect(() => {
    setMode(initialMode);
    setUrl(initialUrl);
    setBroker(initialBroker);
    setPort(String(initialPort));
  }, [initialMode, initialUrl, initialBroker, initialPort]);

  const dirty =
    mode !== initialMode ||
    (mode === "self_hosted" &&
      (url !== initialUrl ||
        broker !== initialBroker ||
        port !== String(initialPort) ||
        apiKey !== ""));
  useDirtyGuard(dirty || busy);

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
        toast.ok(section.message || t("settingsMore.cloud.saved"));
        setApiKey("");
      } else {
        toast.err(section?.message ?? t("settingsMore.cloud.applyFailed"));
      }
    } catch (err) {
      toastFromError(err, t("settingsMore.cloud.applyFailed"));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <div>
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          {t("settingsMore.cloud.posture")}
          <RiskBadge tone="manual" />
        </div>
        <RadioCardGroup
          value={mode}
          onChange={(v) => setMode(v as CloudMode)}
          options={modeOptions}
          columns={3}
        />
      </div>

      {mode === "self_hosted" && (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-4">
            <div className="flex items-center gap-2 text-sm font-semibold">
              {t("settingsMore.cloud.selfHostedEndpoints")}
              <RiskBadge tone="manual" />
            </div>

            <div className="space-y-2">
              <Label htmlFor="cloud-url">{t("settingsMore.cloud.convexUrl")}</Label>
              <Input
                id="cloud-url"
                type="url"
                placeholder={URL_PLACEHOLDER}
                value={url}
                onChange={(e) => {
                  setUrl(e.target.value);
                  setValidationError(null);
                }}
              />
            </div>

            <div className="grid grid-cols-1 sm:grid-cols-[1fr_120px] gap-3">
              <div className="space-y-2">
                <Label htmlFor="cloud-broker">{t("settingsMore.cloud.mqttBroker")}</Label>
                <Input
                  id="cloud-broker"
                  placeholder={BROKER_PLACEHOLDER}
                  value={broker}
                  onChange={(e) => {
                    setBroker(e.target.value);
                    setValidationError(null);
                  }}
                />
              </div>
              <div className="space-y-2">
                <Label htmlFor="cloud-port">{t("settingsMore.cloud.port")}</Label>
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
              <Label htmlFor="cloud-api-key">{t("settingsMore.cloud.apiKey")}</Label>
              <Input
                id="cloud-api-key"
                type="password"
                autoComplete="new-password"
                placeholder={t("settingsMore.cloud.apiKeyPlaceholder")}
                value={apiKey}
                onChange={(e) => {
                  setApiKey(e.target.value);
                  setValidationError(null);
                }}
              />
              <p className="text-[11px] text-muted-foreground">
                {t("settingsMore.cloud.apiKeyHint")}
              </p>
            </div>

            {validationError && (
              <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
                {validationError}
              </div>
            )}
          </CardContent>
        </Card>
      )}

      <CloudflareTokenCard />

      <div className="flex items-center justify-end gap-3">
        {dirty && (
          <span className="text-xs text-muted-foreground">{t("settingsMore.cloud.unsaved")}</span>
        )}
        <Button
          variant="default"
          disabled={!dirty || busy}
          onClick={() => {
            if (validate()) setConfirmOpen(true);
          }}
        >
          {t("settingsMore.cloud.save")}
        </Button>
      </div>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title={t("settingsMore.cloud.confirmTitle")}
        description={
          <Trans
            i18nKey="settingsMore.cloud.confirmBody"
            values={{ mode }}
            components={{ mode: <span className="font-mono font-medium" /> }}
          />
        }
        confirmLabel={t("settingsMore.cloud.apply")}
        destructive
        onConfirm={handleApply}
      />
    </div>
  );
}
