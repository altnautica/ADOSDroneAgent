/**
 * Live MSP telemetry view for a Betaflight/iNav flight controller.
 *
 * The agent is a byte-pipe for MSP (it decodes no MSP telemetry), so this reads
 * attitude / battery / GPS / altitude / RC / sensor state by polling the FC
 * directly over the transparent `ws://<host>:8765/` proxy from the browser.
 * Every value renders "—" until its frame decodes — never zeros-as-live.
 *
 * @module components/fc/msp-sensors-view
 * @license GPL-3.0-only
 */

import { Radio } from "lucide-react";
import { useTranslation } from "react-i18next";

import { Card, CardContent } from "@/components/ui/card";
import { useMspTelemetry } from "@/hooks/use-msp-telemetry";
import type { MspVariant } from "@/lib/fc-firmware";
import { fmtNum, fmtPct, fmtRelativeTime, fmtVoltage } from "@/shared/format";
import { gpsFixLabel } from "@/lib/msp/telemetry-decoders";
import { cn } from "@/lib/utils";

/** A frame older than this reads as stale in the header. */
const STALE_MS = 3000;

function Field({ label, value }: { label: string; value: React.ReactNode }) {
  return (
    <div className="flex items-baseline justify-between border-b border-border/50 py-1.5 last:border-b-0">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className="font-mono text-sm">{value}</span>
    </div>
  );
}

function Panel({
  title,
  children,
}: {
  title: string;
  children: React.ReactNode;
}) {
  return (
    <Card>
      <CardContent className="pt-4 pb-4 space-y-0">
        <div className="text-[11px] font-medium uppercase tracking-wider text-muted-foreground mb-1.5">
          {title}
        </div>
        {children}
      </CardContent>
    </Card>
  );
}

/** RSSI is a raw 0–1023 MSP value; show it as a percentage. */
function rssiPercent(rssi: number): number {
  return Math.max(0, Math.min(100, (rssi / 1023) * 100));
}

export function MspSensorsView({ firmware }: { firmware: MspVariant }) {
  const { t } = useTranslation();
  const tel = useMspTelemetry(firmware);
  const fwLabel = firmware === "betaflight" ? "Betaflight" : "iNav";

  const stale =
    tel?.lastFrameAt != null && Date.now() - tel.lastFrameAt > STALE_MS;

  // Header line: honest link state, never a fake "live" before a frame lands.
  let header: React.ReactNode;
  if (!tel || tel.linkState === "connecting") {
    header = (
      <span className="text-muted-foreground">{t("fc.msp.connecting", { fw: fwLabel })}</span>
    );
  } else if (tel.linkState === "error") {
    header = (
      <span className="text-destructive">
        {tel.error
          ? t("fc.msp.linkErrorDetail", { error: tel.error })
          : t("fc.msp.linkError")}
      </span>
    );
  } else if (tel.linkState === "closed") {
    header = <span className="text-warn">{t("fc.msp.linkClosed")}</span>;
  } else if (tel.lastFrameAt == null) {
    header = (
      <span className="text-muted-foreground">
        {t("fc.msp.waitingFirstFrame", { fw: fwLabel })}
      </span>
    );
  } else {
    header = (
      <span className={stale ? "text-warn" : "text-ok"}>
        {t("fc.msp.live", { fw: fwLabel })}
        {stale ? t("fc.msp.stale", { age: fmtRelativeTime(tel.lastFrameAt) }) : ""}
      </span>
    );
  }

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 text-xs">
        <Radio
          className={cn(
            "h-3.5 w-3.5",
            tel?.linkState === "live" && !stale
              ? "text-ok"
              : tel?.linkState === "error"
                ? "text-destructive"
                : "text-muted-foreground",
          )}
        />
        {header}
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
        {/* Attitude */}
        <Panel title={t("telemetry.sensors.attitude")}>
          <Field label={t("telemetry.sensors.roll")} value={tel?.attitude ? `${fmtNum(tel.attitude.roll, 1)}°` : "—"} />
          <Field label={t("telemetry.sensors.pitch")} value={tel?.attitude ? `${fmtNum(tel.attitude.pitch, 1)}°` : "—"} />
          <Field label={t("telemetry.sensors.yaw")} value={tel?.attitude ? `${fmtNum(tel.attitude.yaw, 0)}°` : "—"} />
        </Panel>

        {/* Battery */}
        <Panel title={t("telemetry.sensors.battery")}>
          <Field label={t("telemetry.sensors.voltage")} value={tel?.analog ? fmtVoltage(tel.analog.voltage) : "—"} />
          <Field label={t("telemetry.sensors.current")} value={tel?.analog ? `${fmtNum(tel.analog.amperage, 2)} A` : "—"} />
          <Field label={t("fc.msp.drawn")} value={tel?.analog ? `${tel.analog.mAhDrawn} mAh` : "—"} />
          <Field label={t("fc.msp.rssi")} value={tel?.analog ? fmtPct(rssiPercent(tel.analog.rssi)) : "—"} />
        </Panel>

        {/* GPS */}
        <Panel title={t("telemetry.sensors.gps")}>
          <Field label={t("fc.msp.fix")} value={tel?.gps ? gpsFixLabel(tel.gps.fixType, firmware) : "—"} />
          <Field label={t("telemetry.sensors.sats")} value={tel?.gps ? String(tel.gps.numSat) : "—"} />
          <Field
            label={
              (tel?.gps?.dopKind ?? (firmware === "betaflight" ? "pdop" : "hdop")) === "pdop"
                ? t("fc.msp.pdop")
                : t("telemetry.sensors.hdop")
            }
            value={tel?.gps?.dop != null ? fmtNum(tel.gps.dop, 2) : "—"}
          />
          <Field
            label={t("telemetry.sensors.position")}
            value={
              tel?.gps && (tel.gps.lat !== 0 || tel.gps.lon !== 0)
                ? `${fmtNum(tel.gps.lat, 5)}, ${fmtNum(tel.gps.lon, 5)}`
                : "—"
            }
          />
          <Field label={t("fc.msp.speed")} value={tel?.gps ? `${fmtNum(tel.gps.speed / 100, 1)} m/s` : "—"} />
        </Panel>

        {/* Altitude */}
        <Panel title={t("fc.msp.altitudeTitle")}>
          <Field label={t("fc.msp.altitude")} value={tel?.altitude ? `${fmtNum(tel.altitude.altitude, 1)} m` : "—"} />
          <Field
            label={t("fc.msp.vario")}
            value={tel?.altitude?.vario != null ? `${fmtNum(tel.altitude.vario / 100, 1)} m/s` : "—"}
          />
        </Panel>

        {/* FC status */}
        <Panel title={t("fc.msp.statusTitle")}>
          <Field
            label={t("telemetry.sensors.armed")}
            value={
              tel?.status ? (
                tel.status.armed ? (
                  <span className="text-warn">{t("telemetry.sensors.armedValue")}</span>
                ) : (
                  <span className="text-muted-foreground">{t("telemetry.sensors.disarmedValue")}</span>
                )
              ) : (
                "—"
              )
            }
          />
          <Field label={t("fc.msp.cpuLoad")} value={tel?.status ? fmtPct(tel.status.cpuLoad) : "—"} />
          <Field label={t("fc.msp.cycleTime")} value={tel?.status ? `${tel.status.cycleTime} µs` : "—"} />
          <Field label={t("fc.msp.i2cErrors")} value={tel?.status ? String(tel.status.i2cErrors) : "—"} />
          {tel?.status?.hardwareFailure != null && (
            <Field
              label={t("fc.msp.hardware")}
              value={
                tel.status.hardwareFailure ? (
                  <span className="text-destructive">{t("fc.msp.fault")}</span>
                ) : (
                  <span className="text-ok">{t("fc.msp.healthy")}</span>
                )
              }
            />
          )}
        </Panel>

        {/* Sensors */}
        <Panel title={t("telemetry.tabs.sensors")}>
          {tel?.sensors ? (
            <div className="flex flex-wrap gap-1.5 pt-1">
              {tel.sensors.map((s) => (
                <span
                  key={s.labelKey}
                  className={cn(
                    "text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border",
                    s.present
                      ? "border-ok/40 text-ok"
                      : "border-muted-foreground/30 text-muted-foreground/60",
                  )}
                >
                  {t(s.labelKey)}
                </span>
              ))}
            </div>
          ) : (
            <p className="text-xs text-muted-foreground pt-1">—</p>
          )}
        </Panel>
      </div>

      {/* RC channels */}
      <Panel title={t("fc.msp.rcChannels")}>
        {tel?.rc && tel.rc.length > 0 ? (
          <div className="grid grid-cols-2 sm:grid-cols-4 lg:grid-cols-8 gap-2 pt-1">
            {tel.rc.map((v, i) => (
              <div key={i} className="text-xs font-mono">
                <div className="text-muted-foreground text-[10px]">{t("fc.msp.channel", { n: i + 1 })}</div>
                <div>{v}</div>
              </div>
            ))}
          </div>
        ) : (
          <p className="text-xs text-muted-foreground pt-1">—</p>
        )}
      </Panel>
    </div>
  );
}
