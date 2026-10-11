import { Trans, useTranslation } from "react-i18next";
import { Link } from "react-router-dom";

import { MspSensorsView } from "@/components/fc/msp-sensors-view";
import { Card, CardContent } from "@/components/ui/card";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { useResource } from "@/hooks/use-resource";
import { useSnapshot } from "@/hooks/use-snapshot";
import { resolveFirmwareType } from "@/lib/fc-firmware";
import { fmtNum, fmtPct, fmtVoltage } from "@/shared/format";

export function SensorsTab() {
  // Route by FC family. A Betaflight/iNav FC speaks MSP: the agent decodes no
  // MSP telemetry (it is a byte-pipe), so the browser polls the FC directly over
  // the transparent proxy for live attitude/battery/GPS/RC/sensors. ArduPilot /
  // PX4 sensors come from the agent's MAVLink vehicle-state snapshot.
  const hb = useHeartbeat();
  const snap = useSnapshot();
  const firmwareType = resolveFirmwareType(hb.data?.fcFirmware, snap.data?.fc?.vehicle);
  if (firmwareType === "betaflight" || firmwareType === "inav") {
    return <MspSensorsView firmware={firmwareType} />;
  }
  return <MavlinkSensors />;
}

// GET /api/telemetry — the router's live vehicle snapshot (attitude in
// radians, battery, GPS, position). Every field is absent or null until the FC
// reports it; nothing here is defaulted.
interface VehicleTelemetry {
  armed?: boolean | null;
  mode?: string | null;
  attitude?: { roll?: number | null; pitch?: number | null; yaw?: number | null };
  battery?: { voltage?: number | null; current?: number | null; remaining?: number | null };
  gps?: { fix_type?: number | null; satellites?: number | null; eph?: number | null };
  position?: { lat?: number | null; lon?: number | null; alt_rel?: number | null };
}

function SensorField({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between border-b border-border/50 py-1.5 last:border-b-0">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="font-mono text-sm">{value}</span>
    </div>
  );
}

export function MavlinkSensors() {
  const { t } = useTranslation();
  const tel = useResource<VehicleTelemetry>("vehicle-telemetry", "/api/telemetry", 1_000);
  const v = tel.data;
  const deg = (rad: number | null | undefined) =>
    rad == null ? "—" : `${fmtNum((rad * 180) / Math.PI, 1)}°`;
  const hasPosition = v?.position?.lat != null && v?.position?.lon != null;

  if (tel.isError && !v) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-destructive">
          {t("telemetry.sensors.readFailed")}
        </CardContent>
      </Card>
    );
  }

  return (
    <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
      <Card>
        <CardContent className="pt-4 pb-4">
          <div className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground mb-1.5">{t("telemetry.sensors.attitude")}</div>
          <SensorField label={t("telemetry.sensors.roll")} value={deg(v?.attitude?.roll)} />
          <SensorField label={t("telemetry.sensors.pitch")} value={deg(v?.attitude?.pitch)} />
          <SensorField label={t("telemetry.sensors.yaw")} value={deg(v?.attitude?.yaw)} />
        </CardContent>
      </Card>
      <Card>
        <CardContent className="pt-4 pb-4">
          <div className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground mb-1.5">{t("telemetry.sensors.battery")}</div>
          <SensorField label={t("telemetry.sensors.voltage")} value={fmtVoltage(v?.battery?.voltage ?? null)} />
          <SensorField
            label={t("telemetry.sensors.current")}
            value={v?.battery?.current != null ? `${fmtNum(v.battery.current, 2)} A` : "—"}
          />
          <SensorField label={t("telemetry.sensors.remaining")} value={fmtPct(v?.battery?.remaining ?? null)} />
        </CardContent>
      </Card>
      <Card>
        <CardContent className="pt-4 pb-4">
          <div className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground mb-1.5">{t("telemetry.sensors.gps")}</div>
          <SensorField label={t("telemetry.sensors.fixType")} value={v?.gps?.fix_type != null ? String(v.gps.fix_type) : "—"} />
          <SensorField label={t("telemetry.sensors.sats")} value={v?.gps?.satellites != null ? String(v.gps.satellites) : "—"} />
          <SensorField label={t("telemetry.sensors.hdop")} value={fmtNum(v?.gps?.eph ?? null, 2)} />
          <SensorField
            label={t("telemetry.sensors.position")}
            value={hasPosition ? `${fmtNum(v!.position!.lat!, 5)}, ${fmtNum(v!.position!.lon!, 5)}` : "—"}
          />
          <SensorField
            label={t("telemetry.sensors.altRel")}
            value={v?.position?.alt_rel != null ? `${fmtNum(v.position.alt_rel, 1)} m` : "—"}
          />
        </CardContent>
      </Card>
      <Card className="md:col-span-2 lg:col-span-3">
        <CardContent className="pt-4 pb-4 grid grid-cols-2 gap-x-6">
          <SensorField label={t("telemetry.sensors.mode")} value={v?.mode ?? "—"} />
          <SensorField
            label={t("telemetry.sensors.armed")}
            value={v?.armed === true ? t("telemetry.sensors.armedValue") : v?.armed === false ? t("telemetry.sensors.disarmedValue") : "—"}
          />
        </CardContent>
      </Card>
      <p className="md:col-span-2 lg:col-span-3 text-[11px] text-muted-foreground">
        <Trans
          i18nKey="telemetry.sensors.editHint"
          components={{ link: <Link to="/telemetry" className="text-primary hover:underline" /> }}
        />
      </p>
    </div>
  );
}
