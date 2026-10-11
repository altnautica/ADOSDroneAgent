import { Plane } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useSnapshot } from "@/hooks/use-snapshot";
import { mspVariant, mspVariantLabel } from "@/lib/fc-firmware";
import { fmtNum, fmtPct, fmtVoltage } from "@/shared/format";

function Row({
  label,
  value,
  mono = true,
}: {
  label: string;
  value: React.ReactNode;
  mono?: boolean;
}) {
  return (
    <div className="flex items-baseline justify-between border-b border-border/50 py-1.5 last:border-b-0">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className={mono ? "font-mono text-sm" : "text-sm"}>{value}</span>
    </div>
  );
}

export function FcPanel() {
  const { t } = useTranslation();
  const snap = useSnapshot();
  const hb = useHeartbeat();
  const fc = snap.data?.fc;

  if (!snap.data) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Plane className="h-3.5 w-3.5" />
            {t("panels.fc.title")}
          </CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-xs text-muted-foreground">{t("panels.fc.connecting")}</p>
        </CardContent>
      </Card>
    );
  }

  // A Betaflight/iNav FC speaks MSP, never a MAVLink heartbeat, so `fc.connected`
  // stays false even though the agent has identified it over the serial link.
  // Render it as a connected FC (identity now; live values on the Sensors tab
  // once the browser MSP poller is running).
  const variant = mspVariant(hb.data?.fcVariant);
  if (!fc?.connected && variant) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Plane className="h-3.5 w-3.5" />
            {t("panels.fc.title")}
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-0">
          <Row label={t("panels.fc.firmware")} value={mspVariantLabel(variant)} mono={false} />
          <Row label={t("panels.fc.telemetry")} value={t("panels.fc.viaMsp")} mono={false} />
          {fc?.fc_port ? (
            <Row label={t("panels.fc.port")} value={`${fc.fc_port} @ ${fc.fc_baud ?? "—"}`} />
          ) : null}
        </CardContent>
      </Card>
    );
  }

  if (!fc?.connected) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Plane className="h-3.5 w-3.5" />
            {t("panels.fc.title")}
          </CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">
            {t("panels.fc.noFcConnected")}
            {fc?.fc_port ? (
              <Trans
                i18nKey="panels.fc.notBound"
                values={{ port: fc.fc_port }}
                components={{ mono: <span className="font-mono" /> }}
              />
            ) : null}
          </p>
        </CardContent>
      </Card>
    );
  }

  const heartbeatLanded = fc.last_heartbeat != null;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Plane className="h-3.5 w-3.5" />
          {t("panels.fc.title")}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-0">
        {!heartbeatLanded && (
          <p className="text-xs text-warn pb-2">
            {t("panels.fc.waitingTelemetry")}
          </p>
        )}
        <Row label={t("panels.fc.vehicle")} value={fc.vehicle || "—"} />
        <Row label={t("panels.fc.firmware")} value={fc.firmware || "—"} />
        <Row
          label={t("panels.fc.mode")}
          value={
            <span className="uppercase">
              {fc.mode || "—"}
              {fc.armed === true ? (
                <span className="ml-2 text-warn">{t("panels.fc.armed")}</span>
              ) : fc.armed === false ? (
                <span className="ml-2 text-muted-foreground">{t("panels.fc.disarmed")}</span>
              ) : (
                <span className="ml-2 text-muted-foreground">{t("panels.fc.armStateUnknown")}</span>
              )}
            </span>
          }
          mono={false}
        />
        <Row
          label={t("panels.fc.gps")}
          value={
            fc.gps.satellites_visible != null
              ? t("panels.fc.gpsValue", { sats: fc.gps.satellites_visible, hdop: fmtNum(fc.gps.hdop, 1) })
              : "—"
          }
        />
        <Row
          label={t("panels.fc.battery")}
          value={
            fc.battery.voltage != null
              ? `${fmtVoltage(fc.battery.voltage)} · ${fmtPct(fc.battery.remaining)}`
              : "—"
          }
        />
        <Row label={t("panels.fc.rc")} value={fmtPct(fc.rc)} />
        <Row label={t("panels.fc.port")} value={fc.fc_port ? `${fc.fc_port} @ ${fc.fc_baud ?? "—"}` : "—"} />
      </CardContent>
    </Card>
  );
}
