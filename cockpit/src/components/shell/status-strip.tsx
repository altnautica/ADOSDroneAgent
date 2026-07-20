// L3 — the persistent top status strip. Present on every screen so link health
// is never more than a glance away. Reads the shared telemetry; null readings
// render an em-dash, never a fabricated zero. When `floating` (over the Feed video) it is
// translucent; otherwise it is a solid charcoal bar.

import { Wifi, WifiOff } from "lucide-react";

import { useTelemetryContext } from "@/hooks/telemetry-context";
import { fmtDbm, fmtTemp, DASH } from "@/lib/format";
import { cn } from "@/lib/utils";

function stateDotColor(state: string | undefined): string {
  switch (state) {
    case "connected":
      return "bg-ok";
    case "degraded":
    case "rf_unverified":
      return "bg-warn";
    case "connecting":
      return "bg-muted-foreground";
    default:
      return "bg-err";
  }
}

function Item({
  label,
  children,
}: {
  label: string;
  children: React.ReactNode;
}) {
  return (
    <div className="flex items-baseline gap-[0.3rem] whitespace-nowrap">
      <span className="text-[0.62rem] uppercase tracking-wide text-muted-foreground">
        {label}
      </span>
      <span className="text-[0.82rem] text-surface-foreground">{children}</span>
    </div>
  );
}

export function StatusStrip({ floating = false }: { floating?: boolean }) {
  const { status, stale } = useTelemetryContext();
  const link = status?.link;
  const role = status?.role?.current;
  const uplink = status?.network?.uplink_type;
  const uplinkReachable = status?.network?.uplink_reachable;
  const paired = Boolean(status?.paired_drone?.device_id);
  const temp = status?.system?.temp_c;

  return (
    <div
      className={cn(
        "flex items-center gap-[0.9rem] overflow-x-auto px-[0.75rem] py-[0.4rem]",
        floating
          ? "pointer-events-auto bg-background/60 backdrop-blur-sm"
          : "border-b border-border bg-surface/70",
        stale && "opacity-70",
      )}
    >
      <div className="flex items-center gap-[0.4rem]">
        <span
          className={cn("h-[0.6rem] w-[0.6rem] rounded-full", stateDotColor(link?.state))}
          aria-hidden
        />
        <Item label="Link">{fmtDbm(link?.rssi_dbm)}</Item>
      </div>
      <Item label="Role">{role ?? DASH}</Item>
      <Item label="Uplink">
        <span className="inline-flex items-center gap-[0.25rem]">
          {uplinkReachable ? (
            <Wifi className="h-[0.85rem] w-[0.85rem] text-ok" aria-hidden />
          ) : (
            <WifiOff className="h-[0.85rem] w-[0.85rem] text-muted-foreground" aria-hidden />
          )}
          {uplink ?? DASH}
        </span>
      </Item>
      <Item label="Pair">
        <span className={paired ? "text-ok" : "text-muted-foreground"}>
          {paired ? "linked" : "none"}
        </span>
      </Item>
      <div className="ml-auto flex items-center gap-[0.9rem]">
        <Item label="Temp">{fmtTemp(temp)}</Item>
        {stale ? <span className="text-[0.72rem] text-warn">stale</span> : null}
      </div>
    </div>
  );
}
