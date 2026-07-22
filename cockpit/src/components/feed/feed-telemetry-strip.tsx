// The bottom HUD readout strip: flight mode, heading, GPS fix, satellite count,
// home distance, and battery (percent, plus voltage and current when the vehicle
// supplies them). Flight fields come from the shared flight telemetry; mode /
// sats / battery fall back to the ground-station status composite's paired-drone
// block when the vehicle snapshot lacks them. Every cell shows a dash when its
// value is unknown — nothing is fabricated (home distance has no source in the
// vehicle snapshot today, so it reads a dash until the agent supplies one).

import { useFlightTelemetryContext } from "@/hooks/flight-telemetry-context";
import { useTelemetryContext } from "@/hooks/telemetry-context";
import { DASH, fmtGpsFix, fmtHeading, fmtMeters, fmtSats } from "@/lib/format";
import { cn } from "@/lib/utils";

function Cell({
  label,
  value,
  accent,
}: {
  label: string;
  value: string;
  accent?: string;
}) {
  return (
    <div className="flex min-w-[3rem] flex-col items-center">
      <span className="text-[0.55rem] uppercase tracking-wide text-muted-foreground">
        {label}
      </span>
      <span className={cn("font-mono text-[0.85rem] font-semibold", accent ?? "text-surface-foreground")}>
        {value}
      </span>
    </div>
  );
}

/** Battery percent, treating the MAVLink "unknown" sentinel (-1) as no reading. */
function batteryPct(remaining: number | null | undefined): number | null {
  return remaining != null && remaining >= 0 ? remaining : null;
}

/** Build the battery readout: percent, plus voltage and current, showing only
 *  the parts the vehicle actually supplies (never a fabricated value).
 *
 *  MAVLink SYS_STATUS reports `voltage_battery = 65535` mV and
 *  `current_battery = -1` cA when the flight controller has NO battery monitor
 *  (a common voltage-only / no-power-module setup). The agent divides those
 *  through (÷1000 / ÷100) and passes them along, so an unguarded display shows a
 *  fabricated "65.5V" / "-0.0A". Reverse the exact conversion to reject the exact
 *  sentinel — this never false-hides a real high-voltage pack (a genuine 65.5V
 *  reading is 65500 mV, not the 65535 sentinel). */
function batteryValue(
  pct: number | null,
  voltage: number | null | undefined,
  current: number | null | undefined,
): string {
  const parts: string[] = [];
  if (pct != null) parts.push(`${Math.round(pct)}%`);
  if (typeof voltage === "number" && Number.isFinite(voltage)) {
    const mv = Math.round(voltage * 1000);
    if (mv > 0 && mv !== 65535) parts.push(`${voltage.toFixed(1)}V`);
  }
  if (typeof current === "number" && Number.isFinite(current)) {
    // -1 cA is the unknown sentinel; a real discharge current is non-negative.
    if (Math.round(current * 100) >= 0) parts.push(`${current.toFixed(1)}A`);
  }
  return parts.length > 0 ? parts.join(" ") : DASH;
}

export function FeedTelemetryStrip() {
  const { telemetry, live, stale } = useFlightTelemetryContext();
  const { status } = useTelemetryContext();
  const drone = status?.paired_drone;

  const mode = telemetry?.mode ?? drone?.fc_mode ?? null;
  const armed = telemetry?.armed === true;
  const heading = telemetry?.position?.heading ?? null;
  const fix = telemetry?.gps?.fix_type ?? null;
  const sats = telemetry?.gps?.satellites ?? drone?.gps_sats ?? null;
  const dist = telemetry?.home_distance ?? null;
  const batt = batteryPct(telemetry?.battery?.remaining) ?? drone?.battery_pct ?? null;
  const battAccent = batt == null ? undefined : batt <= 15 ? "text-err" : batt <= 30 ? "text-warn" : undefined;

  return (
    <div
      className={cn(
        "pointer-events-none flex items-center gap-[0.9rem] rounded-lg bg-background/55 px-[0.8rem] py-[0.3rem] backdrop-blur-sm",
        (stale || !live) && "opacity-80",
      )}
    >
      <Cell
        label="Mode"
        value={mode ?? DASH}
        accent={mode ? (armed ? "text-err" : "text-ok") : undefined}
      />
      <Cell label="Hdg" value={fmtHeading(heading)} />
      <Cell label="GPS" value={fmtGpsFix(fix)} />
      <Cell label="Sats" value={fmtSats(sats)} />
      <Cell label="Dist" value={fmtMeters(dist)} />
      <Cell
        label="Batt"
        value={batteryValue(batt, telemetry?.battery?.voltage, telemetry?.battery?.current)}
        accent={battAccent}
      />
    </div>
  );
}
