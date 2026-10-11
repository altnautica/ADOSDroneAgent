import { useTranslation } from "react-i18next";

import { Card, CardContent } from "@/components/ui/card";
import {
  ConfigEnumField,
  ConfigNumberField,
  ConfigTextField,
  ReadRow,
} from "@/components/settings/config-fields";
import { useConfig } from "@/hooks/use-config";

// Example endpoint values, not copy.
const NETWORK_TARGET_EXAMPLE = "udp:127.0.0.1:14550";
const SERIAL_PORT_EXAMPLE = "/dev/ttyACM0";

const SOURCE_OPTIONS = [
  {
    value: "auto" as const,
    labelKey: "settings.mavlink.sourceAutoLabel",
    descriptionKey: "settings.mavlink.sourceAutoDescription",
  },
  {
    value: "serial" as const,
    labelKey: "settings.mavlink.sourceSerialLabel",
    descriptionKey: "settings.mavlink.sourceSerialDescription",
  },
  {
    value: "udp" as const,
    labelKey: "settings.mavlink.sourceUdpLabel",
    descriptionKey: "settings.mavlink.sourceUdpDescription",
  },
  {
    value: "tcp" as const,
    labelKey: "settings.mavlink.sourceTcpLabel",
    descriptionKey: "settings.mavlink.sourceTcpDescription",
  },
] as const;

export function MavlinkSettings() {
  const { t } = useTranslation();
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">{t("settings.shared.readingConfig")}</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        {t("settings.mavlink.readFailed")}
      </div>
    );
  }

  const mav = config.data?.mavlink;
  if (!mav) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          {t("settings.mavlink.notExposed")}
        </CardContent>
      </Card>
    );
  }

  const source = mav.source ?? "auto";
  const isNetwork = source === "udp" || source === "tcp";
  const endpoints = mav.endpoints ?? [];

  return (
    <div className="space-y-6">
      {/* FC transport source. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-4">
          <div>
            <div className="text-sm font-semibold">{t("settings.mavlink.transportTitle")}</div>
            <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
              {t("settings.mavlink.transportHint")}
            </p>
          </div>
          <ConfigEnumField
            configKey="mavlink.source"
            value={source}
            options={SOURCE_OPTIONS.map((o) => ({
              value: o.value,
              label: t(o.labelKey),
              description: t(o.descriptionKey),
            }))}
            columns={2}
          />
        </CardContent>
      </Card>

      {/* Serial / network target + baud. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <ConfigTextField
            configKey="mavlink.serial_port"
            id="mavlink-serial-port"
            label={isNetwork ? t("settings.mavlink.networkTarget") : t("settings.mavlink.serialPort")}
            hint={
              isNetwork
                ? t("settings.mavlink.networkTargetHint")
                : t("settings.mavlink.serialPortHint")
            }
            placeholder={isNetwork ? NETWORK_TARGET_EXAMPLE : SERIAL_PORT_EXAMPLE}
            value={mav.serial_port}
            disabled={source === "auto"}
          />
          <ConfigNumberField
            configKey="mavlink.baud_rate"
            id="mavlink-baud"
            label={t("settings.mavlink.baudRate")}
            hint={t("settings.mavlink.baudRateHint")}
            value={mav.baud_rate}
            integer
            min={1200}
            disabled={source === "auto" || isNetwork}
          />
        </CardContent>
      </Card>

      {/* Agent MAVLink identity. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <div>
            <div className="text-sm font-semibold">{t("settings.mavlink.identityTitle")}</div>
            <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
              {t("settings.mavlink.identityHint")}
            </p>
          </div>
          <ConfigNumberField
            configKey="mavlink.system_id"
            id="mavlink-system-id"
            label={t("settings.mavlink.systemId")}
            value={mav.system_id}
            integer
            min={1}
            max={255}
          />
          <ConfigNumberField
            configKey="mavlink.component_id"
            id="mavlink-component-id"
            label={t("settings.mavlink.componentId")}
            value={mav.component_id}
            integer
            min={1}
            max={255}
          />
        </CardContent>
      </Card>

      {/* Advertised endpoints — read-only (the list is not a single-key write). */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-2">
          <div className="text-sm font-semibold mb-1">{t("settings.mavlink.endpointsTitle")}</div>
          {endpoints.length === 0 ? (
            <p className="text-[11px] text-muted-foreground">
              {t("settings.mavlink.noEndpoints")}
            </p>
          ) : (
            <div className="space-y-1">
              {endpoints.map((ep, idx) => (
                <ReadRow
                  key={`${ep.type ?? "endpoint"}-${ep.port ?? idx}`}
                  label={ep.type ?? t("settings.mavlink.endpointFallback")}
                  value={`${ep.host ?? "0.0.0.0"}:${ep.port ?? "?"}${
                    ep.enabled === false ? t("settings.mavlink.disabledSuffix") : ""
                  }`}
                />
              ))}
            </div>
          )}
          <p className="text-[11px] text-muted-foreground pt-1">
            {t("settings.mavlink.endpointsHint")}
          </p>
        </CardContent>
      </Card>

      {/* Honest absence: signing + rates + WS auth are elsewhere / not config. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1.5">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            {t("settings.mavlink.notConfiguredTitle")}
          </div>
          <p className="text-xs text-muted-foreground leading-relaxed">
            {t("settings.mavlink.notConfiguredBody")}
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
