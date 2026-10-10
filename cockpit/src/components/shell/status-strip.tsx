// L3 — the persistent top status strip, present on every screen so the most
// operationally-important state is never more than a glance away (this is the
// primary surface for a DISPLAY-ONLY ground operator with no keyboard/touch). It
// is laid out in three prioritised zones:
//
//   LEFT  — RF LINK: the one-glance link verdict + RSSI (with bars) + loss + SNR
//           + channel + bitrate. The leading dot and verdict are driven by the
//           agent's link_diag, so a link that is "connected" but actually deaf
//           reads red, not green.
//   RIGHT — BOX / REACH: role, uplink + reachability, pair state, CPU, a
//           threshold-coloured temperature, a wall clock, a recording dot, and a
//           stale badge when the poll is failing.
//
// Every null reading renders an em-dash, never a fabricated zero; a verdict the
// agent has not classified simply omits its chip. Lower-priority items reveal on
// wider panels (md:/lg:) so the essentials always fit an 800px kiosk without a
// scrollbar the operator cannot reach.

import { Wifi, WifiOff } from "lucide-react";
import type { ReactNode } from "react";

import { SignalBars } from "@/components/shell/signal-bars";
import { WallClock } from "@/components/shell/wall-clock";
import { Dot, toneClass, type Tone } from "@/components/ui/data";
import { useStatusStore } from "@/stores/status-store";
import { fmtChannel, fmtDbm, fmtMbps, fmtPct, fmtTemp, DASH } from "@/shared/format";
import { fmtDb, fmtKbpsAsMbps, fmtLossPct } from "@/lib/format-status";
import { linkDiagView, type LinkDiagView } from "@/lib/link-diag";
import type { GsStatus } from "@/lib/types";
import { cn } from "@/lib/utils";

/** Map a bare link state string to a tone, the fallback when the agent has not
 *  yet classified the link with a verdict. Only the states that name a fault
 *  are coloured; an unknown or absent state is neutral, never red. */
function stateTone(state: string | null | undefined): Tone {
  switch (state) {
    case "connected":
      return "ok";
    case "degraded":
    case "rf_unverified":
      return "warn";
    case "failed":
    case "error":
      return "err";
    default:
      return "muted";
  }
}

function Item({
  label,
  children,
  className,
}: {
  label: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("flex items-baseline gap-[0.3rem] whitespace-nowrap", className)}>
      <span className="text-[0.75rem] uppercase tracking-wide text-muted-foreground">
        {label}
      </span>
      <span className="text-[0.82rem] text-surface-foreground">{children}</span>
    </div>
  );
}

/** The one-glance RF verdict pill (icon + short label), tinted by tone. */
function VerdictChip({ view }: { view: LinkDiagView }) {
  const Icon = view.icon;
  const tint =
    view.tone === "ok"
      ? "bg-ok/15"
      : view.tone === "warn"
        ? "bg-warn/15"
        : view.tone === "err"
          ? "bg-err/15"
          : "bg-muted-foreground/15";
  return (
    <span
      className={cn(
        "inline-flex items-center gap-[0.28rem] rounded-full px-[0.42rem] py-[0.08rem]",
        tint,
      )}
    >
      <Icon className={cn("h-[0.85rem] w-[0.85rem]", toneClass(view.tone))} aria-hidden />
      <span className={cn("text-[0.75rem] font-medium", toneClass(view.tone))}>{view.label}</span>
    </span>
  );
}

/** LEFT zone — the RF link. */
function LinkZone({ link }: { link: GsStatus["link"] | undefined }) {
  const view = linkDiagView(link?.link_diag);
  const dotTone = view ? view.tone : stateTone(link?.state);
  const loss = link?.loss_percent ?? null;
  const bitrate =
    link?.bitrate_mbps != null ? fmtMbps(link.bitrate_mbps) : fmtKbpsAsMbps(link?.bitrate_kbps);

  return (
    <div className="flex items-center gap-[0.6rem]">
      <Dot tone={dotTone} />
      {view ? <VerdictChip view={view} /> : null}
      <span className="inline-flex items-center gap-[0.32rem]">
        <SignalBars rssi={link?.rssi_dbm} tone={dotTone} />
        <span className="font-mono text-[0.8rem] text-surface-foreground">{fmtDbm(link?.rssi_dbm)}</span>
      </span>
      <Item label="Loss">
        <span className={loss != null && loss > 5 ? "text-warn" : undefined}>{fmtLossPct(loss)}</span>
      </Item>
      <Item label="SNR" className="hidden md:flex">
        {fmtDb(link?.snr_db)}
      </Item>
      <Item label="Ch" className="hidden lg:flex">
        {fmtChannel(link?.channel)}
      </Item>
      <Item label="Rate" className="hidden lg:flex">
        {bitrate}
      </Item>
    </div>
  );
}

/** RIGHT zone — the box + reach state. */
function BoxZone({ status, stale }: { status: GsStatus | null; stale: boolean }) {
  const role = status?.role?.current;
  const uplink = status?.network?.uplink_type;
  const uplinkReachable = status?.network?.uplink_reachable;
  const paired = Boolean(status?.paired_drone?.device_id);
  const cpu = status?.system?.cpu_pct ?? null;
  const temp = status?.system?.temp_c ?? null;
  const recording = status?.recording === true || status?.video?.recording === true;
  const tempClass = temp == null ? undefined : temp >= 80 ? "text-err" : temp >= 70 ? "text-warn" : undefined;

  return (
    <div className="flex items-center gap-[0.9rem]">
      {recording ? (
        <span className="inline-flex items-center gap-[0.28rem] text-err">
          <span className="h-[0.5rem] w-[0.5rem] animate-pulse rounded-full bg-err" aria-hidden />
          <span className="text-[0.75rem] font-medium">REC</span>
        </span>
      ) : null}
      {role ? (
        <Item label="Role" className="hidden lg:flex">
          {role}
        </Item>
      ) : null}
      <Item label="Uplink" className="hidden md:flex">
        <span className="inline-flex items-center gap-[0.25rem]">
          {/* Reachability is three-state: only badge it when the node actually
              reports it. Absent (undefined) is UNKNOWN, not "not reachable", so
              it shows no icon rather than a misleading offline badge. */}
          {uplinkReachable === true ? (
            <Wifi className="h-[0.85rem] w-[0.85rem] text-ok" aria-hidden />
          ) : uplinkReachable === false ? (
            <WifiOff className="h-[0.85rem] w-[0.85rem] text-muted-foreground" aria-hidden />
          ) : null}
          {uplink ?? DASH}
        </span>
      </Item>
      <Item label="Pair">
        <span className={paired ? "text-ok" : "text-muted-foreground"}>
          {paired ? "linked" : "none"}
        </span>
      </Item>
      <Item label="CPU" className="hidden lg:flex">
        {fmtPct(cpu)}
      </Item>
      <Item label="Temp">
        <span className={tempClass}>{fmtTemp(temp)}</span>
      </Item>
      <span className="text-[0.78rem] text-surface-foreground">
        <WallClock />
      </span>
      {stale ? <span className="text-[0.75rem] text-warn">stale</span> : null}
    </div>
  );
}

export function StatusStrip({ floating = false }: { floating?: boolean }) {
  const status = useStatusStore((s) => s.status);
  const stale = useStatusStore((s) => s.stale);

  return (
    <div
      className={cn(
        "flex items-center gap-[0.9rem] overflow-hidden px-[0.75rem] py-[0.4rem]",
        floating
          ? "pointer-events-auto bg-background/60 backdrop-blur-sm"
          : "border-b border-border bg-surface/70",
        stale && "opacity-70",
      )}
    >
      <LinkZone link={status?.link} />
      <div className="flex-1" />
      <BoxZone status={status} stale={stale} />
    </div>
  );
}
