// L2 — the lightweight HUD overlay drawn on top of the video. Corner chips
// read the shared telemetry: link RSSI + state, channel + loss, and the paired
// drone's mode/battery/GPS when one is paired. Null readings render an em-dash
// (never a fabricated zero). Purely informational; it does not
// capture pointer events so the feed stays interactive underneath.

import { useTelemetryContext } from "@/hooks/telemetry-context";
import { fmtChannel, fmtDbm, fmtPct, DASH } from "@/lib/format";
import { cn } from "@/lib/utils";

function linkStateColor(state: string | undefined): string {
  switch (state) {
    case "connected":
      return "text-ok";
    case "degraded":
    case "rf_unverified":
      return "text-warn";
    case "connecting":
      return "text-muted-foreground";
    default:
      return "text-err";
  }
}

function Chip({
  className,
  children,
}: {
  className?: string;
  children: React.ReactNode;
}) {
  return (
    <div
      className={cn(
        "pointer-events-none rounded-md bg-background/55 px-[0.6rem] py-[0.35rem] font-mono text-[0.78rem] backdrop-blur-sm",
        className,
      )}
    >
      {children}
    </div>
  );
}

export function HudLayer() {
  const { status, stale } = useTelemetryContext();
  const link = status?.link;
  const drone = status?.paired_drone;
  const paired = Boolean(drone?.device_id);

  return (
    <div className="pointer-events-none absolute inset-0 z-10">
      {/* top-left: link RSSI + state */}
      <div className="absolute left-[0.6rem] top-[0.6rem] flex flex-col gap-[0.3rem]">
        <Chip className={cn(stale && "opacity-60")}>
          <span className="text-muted-foreground">LINK </span>
          <span className="text-surface-foreground">{fmtDbm(link?.rssi_dbm)}</span>
          <span className={cn("ml-[0.4rem]", linkStateColor(link?.state))}>
            {link?.state ?? DASH}
          </span>
        </Chip>
      </div>

      {/* top-right: channel + loss */}
      <div className="absolute right-[0.6rem] top-[0.6rem] flex flex-col items-end gap-[0.3rem]">
        <Chip className={cn(stale && "opacity-60")}>
          <span className="text-surface-foreground">{fmtChannel(link?.channel)}</span>
          <span className="ml-[0.4rem] text-muted-foreground">
            loss {link?.loss_percent == null ? DASH : `${link.loss_percent}%`}
          </span>
        </Chip>
      </div>

      {/* bottom-left: paired drone summary */}
      {paired ? (
        <div className="absolute bottom-[0.6rem] left-[0.6rem]">
          <Chip>
            <span className="text-muted-foreground">MODE </span>
            <span className="text-surface-foreground">{drone?.fc_mode ?? DASH}</span>
            <span className="ml-[0.5rem] text-muted-foreground">BAT </span>
            <span className="text-surface-foreground">{fmtPct(drone?.battery_pct)}</span>
            <span className="ml-[0.5rem] text-muted-foreground">SAT </span>
            <span className="text-surface-foreground">{drone?.gps_sats ?? DASH}</span>
          </Chip>
        </div>
      ) : null}

      {stale ? (
        <div className="absolute bottom-[0.6rem] right-[0.6rem]">
          <Chip className="text-warn">telemetry stale</Chip>
        </div>
      ) : null}
    </div>
  );
}
