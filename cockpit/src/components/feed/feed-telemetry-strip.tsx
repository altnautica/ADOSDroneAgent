// The bottom HUD readout strip: flight mode, heading, satellite count, home
// distance, and battery. Flight fields come from the shared flight telemetry;
// mode / sats / battery fall back to the ground-station status composite's
// paired-drone block when the vehicle snapshot lacks them. Every cell shows a
// dash when its value is unknown — nothing is fabricated (home distance has no
// source in the vehicle snapshot today, so it reads a dash until the agent
// supplies one).

import { useFlightTelemetryContext } from "@/hooks/flight-telemetry-context";
import { useTelemetryContext } from "@/hooks/telemetry-context";
import { DASH, fmtHeading, fmtMeters, fmtPct, fmtSats } from "@/lib/format";
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

export function FeedTelemetryStrip() {
  const { telemetry, live, stale } = useFlightTelemetryContext();
  const { status } = useTelemetryContext();
  const drone = status?.paired_drone;

  const mode = telemetry?.mode ?? drone?.fc_mode ?? null;
  const armed = telemetry?.armed === true;
  const heading = telemetry?.position?.heading ?? null;
  const sats = telemetry?.gps?.satellites ?? drone?.gps_sats ?? null;
  const dist = telemetry?.home_distance ?? null;
  const batt =
    batteryPct(telemetry?.battery?.remaining) ?? drone?.battery_pct ?? null;

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
      <Cell label="Sats" value={fmtSats(sats)} />
      <Cell label="Dist" value={fmtMeters(dist)} />
      <Cell label="Batt" value={fmtPct(batt)} />
    </div>
  );
}
