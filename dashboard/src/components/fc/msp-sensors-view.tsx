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

import { Card, CardContent } from "@/components/ui/card";
import { useMspTelemetry } from "@/hooks/use-msp-telemetry";
import type { MspVariant } from "@/lib/fc-firmware";
import { fmtNum, fmtPercent, fmtRelativeTime, fmtVoltage } from "@/lib/format";
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
  const t = useMspTelemetry(firmware);
  const fwLabel = firmware === "betaflight" ? "Betaflight" : "iNav";

  const stale =
    t?.lastFrameAt != null && Date.now() - t.lastFrameAt > STALE_MS;

  // Header line: honest link state, never a fake "live" before a frame lands.
  let header: React.ReactNode;
  if (!t || t.linkState === "connecting") {
    header = (
      <span className="text-muted-foreground">connecting to {fwLabel} over MSP…</span>
    );
  } else if (t.linkState === "error") {
    header = (
      <span className="text-destructive">
        MSP link error{t.error ? `: ${t.error}` : ""}
      </span>
    );
  } else if (t.linkState === "closed") {
    header = <span className="text-warn">MSP link closed — reconnecting…</span>;
  } else if (t.lastFrameAt == null) {
    header = (
      <span className="text-muted-foreground">
        connected — waiting for the first {fwLabel} frame…
      </span>
    );
  } else {
    header = (
      <span className={stale ? "text-warn" : "text-ok"}>
        live from {fwLabel} over MSP
        {stale ? ` · stale (${fmtRelativeTime(t.lastFrameAt)})` : ""}
      </span>
    );
  }

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 text-xs">
        <Radio
          className={cn(
            "h-3.5 w-3.5",
            t?.linkState === "live" && !stale
              ? "text-ok"
              : t?.linkState === "error"
                ? "text-destructive"
                : "text-muted-foreground",
          )}
        />
        {header}
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">
        {/* Attitude */}
        <Panel title="Attitude">
          <Field label="roll" value={t?.attitude ? `${fmtNum(t.attitude.roll, 1)}°` : "—"} />
          <Field label="pitch" value={t?.attitude ? `${fmtNum(t.attitude.pitch, 1)}°` : "—"} />
          <Field label="yaw" value={t?.attitude ? `${fmtNum(t.attitude.yaw, 0)}°` : "—"} />
        </Panel>

        {/* Battery */}
        <Panel title="Battery">
          <Field label="voltage" value={t?.analog ? fmtVoltage(t.analog.voltage) : "—"} />
          <Field label="current" value={t?.analog ? `${fmtNum(t.analog.amperage, 2)} A` : "—"} />
          <Field label="drawn" value={t?.analog ? `${t.analog.mAhDrawn} mAh` : "—"} />
          <Field label="rssi" value={t?.analog ? fmtPercent(rssiPercent(t.analog.rssi)) : "—"} />
        </Panel>

        {/* GPS */}
        <Panel title="GPS">
          <Field label="fix" value={t?.gps ? gpsFixLabel(t.gps.fixType) : "—"} />
          <Field label="sats" value={t?.gps ? String(t.gps.numSat) : "—"} />
          <Field label="hdop" value={t?.gps?.hdop != null ? fmtNum(t.gps.hdop, 2) : "—"} />
          <Field
            label="position"
            value={
              t?.gps && (t.gps.lat !== 0 || t.gps.lon !== 0)
                ? `${fmtNum(t.gps.lat, 5)}, ${fmtNum(t.gps.lon, 5)}`
                : "—"
            }
          />
          <Field label="speed" value={t?.gps ? `${fmtNum(t.gps.speed / 100, 1)} m/s` : "—"} />
        </Panel>

        {/* Altitude */}
        <Panel title="Altitude">
          <Field label="altitude" value={t?.altitude ? `${fmtNum(t.altitude.altitude, 1)} m` : "—"} />
          <Field label="vario" value={t?.altitude ? `${fmtNum(t.altitude.vario / 100, 1)} m/s` : "—"} />
        </Panel>

        {/* FC status */}
        <Panel title="Status">
          <Field
            label="armed"
            value={
              t?.status ? (
                t.status.armed ? (
                  <span className="text-warn">ARMED</span>
                ) : (
                  <span className="text-muted-foreground">disarmed</span>
                )
              ) : (
                "—"
              )
            }
          />
          <Field label="cpu load" value={t?.status ? fmtPercent(t.status.cpuLoad) : "—"} />
          <Field label="cycle time" value={t?.status ? `${t.status.cycleTime} µs` : "—"} />
          <Field label="i2c errors" value={t?.status ? String(t.status.i2cErrors) : "—"} />
          {t?.status?.hardwareFailure != null && (
            <Field
              label="hardware"
              value={
                t.status.hardwareFailure ? (
                  <span className="text-destructive">fault</span>
                ) : (
                  <span className="text-ok">healthy</span>
                )
              }
            />
          )}
        </Panel>

        {/* Sensors */}
        <Panel title="Sensors">
          {t?.sensors ? (
            <div className="flex flex-wrap gap-1.5 pt-1">
              {t.sensors.map((s) => (
                <span
                  key={s.label}
                  className={cn(
                    "text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border",
                    s.present
                      ? "border-ok/40 text-ok"
                      : "border-muted-foreground/30 text-muted-foreground/60",
                  )}
                >
                  {s.label}
                </span>
              ))}
            </div>
          ) : (
            <p className="text-xs text-muted-foreground pt-1">—</p>
          )}
        </Panel>
      </div>

      {/* RC channels */}
      <Panel title="RC channels">
        {t?.rc && t.rc.length > 0 ? (
          <div className="grid grid-cols-2 sm:grid-cols-4 lg:grid-cols-8 gap-2 pt-1">
            {t.rc.map((v, i) => (
              <div key={i} className="text-xs font-mono">
                <div className="text-muted-foreground text-[10px]">ch{i + 1}</div>
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
