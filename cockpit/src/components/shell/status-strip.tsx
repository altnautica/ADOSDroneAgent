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

import { MoreHorizontal, Wifi, WifiOff } from "lucide-react";
import { useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";

import { SignalBars } from "@/components/shell/signal-bars";
import { WallClock } from "@/components/shell/wall-clock";
import { Dot } from "@/components/ui/data";
import { toneClass, type Tone } from "@/lib/tone";
import { CHROME } from "@/lib/cockpit-layout";
import { useStatusStore } from "@/stores/status-store";
import { fmtChannel, fmtDbm, fmtMbps, fmtPct, fmtTemp, DASH } from "@/shared/format";
import { fmtDb, fmtKbpsAsMbps, fmtLossPct } from "@/lib/format-status";
import { uplinkView } from "@/shared/api/uplink-handover";
import { linkDiagView, type LinkDiagView } from "@/lib/link-diag";
import type { GsStatus } from "@/lib/types";
import { cn } from "@/lib/utils";
import "@/shared/i18n";
import { linkMeasured, stripPaired } from "./strip-view";

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
export function VerdictChip({ view }: { view: LinkDiagView }) {
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
        "inline-flex shrink-0 items-center gap-[0.28rem] whitespace-nowrap rounded-full px-[0.42rem] py-[0.08rem]",
        tint,
      )}
    >
      <Icon className={cn("h-[0.85rem] w-[0.85rem]", toneClass(view.tone))} aria-hidden />
      <span className={cn("text-[0.75rem] font-medium", toneClass(view.tone))}>{view.label}</span>
    </span>
  );
}

/** LEFT zone — the RF link: verdict, RSSI and loss only. */
function LinkZone({ link }: { link: GsStatus["link"] | undefined }) {
  const { t } = useTranslation();
  const view = linkDiagView(link?.link_diag);
  const dotTone = view ? view.tone : stateTone(link?.state);
  // Nothing decoded: loss is not measured, so show a dash, never 0.0%.
  const loss = linkMeasured(link) ? (link?.loss_percent ?? null) : null;

  return (
    <div className="flex shrink-0 items-center gap-[0.6rem] whitespace-nowrap">
      <Dot tone={dotTone} />
      {view ? <VerdictChip view={view} /> : null}
      <span className="inline-flex items-center gap-[0.32rem]">
        <SignalBars rssi={link?.rssi_dbm} tone={dotTone} />
        <span className="font-mono text-[0.8rem] text-surface-foreground">{fmtDbm(link?.rssi_dbm)}</span>
      </span>
      <Item label={t("cockpit.shell.strip.loss")}>
        <span className={loss != null && loss > 5 ? "text-warn" : undefined}>{fmtLossPct(loss)}</span>
      </Item>
    </div>
  );
}

/** The secondary readings (SNR, channel, rate, role, uplink, CPU), shown in a
 *  panel under the strip on tap so the strip never overflows. */
function MoreZone({ status }: { status: GsStatus | null }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const link = status?.link;
  const measured = linkMeasured(link);
  const bitrate = !measured
    ? DASH
    : link?.bitrate_mbps != null
      ? fmtMbps(link.bitrate_mbps)
      : fmtKbpsAsMbps(link?.bitrate_kbps);
  const role = status?.role?.current;
  const uplink = status?.network?.uplink_type;
  const uplinkReachable = status?.network?.uplink_reachable;

  return (
    <div className="relative shrink-0">
      <button
        type="button"
        className="inline-flex h-[2rem] items-center rounded px-[0.4rem] text-muted-foreground hover:text-surface-foreground"
        aria-expanded={open}
        aria-label={t("cockpit.shell.strip.more")}
        onClick={() => setOpen((v) => !v)}
      >
        <MoreHorizontal className="h-[1rem] w-[1rem]" aria-hidden />
      </button>
      {open ? (
        <div className="absolute right-0 top-full z-50 mt-[0.3rem] flex flex-col gap-[0.35rem] rounded-md border border-border bg-surface px-[0.75rem] py-[0.5rem] shadow-lg">
          <Item label={t("cockpit.shell.strip.snr")}>{measured ? fmtDb(link?.snr_db) : DASH}</Item>
          <Item label={t("cockpit.shell.strip.channel")}>{fmtChannel(link?.channel)}</Item>
          <Item label={t("cockpit.shell.strip.rate")}>{bitrate}</Item>
          {role ? <Item label={t("cockpit.shell.strip.role")}>{role}</Item> : null}
          <Item label={t("cockpit.shell.strip.uplink")}>
            <span className="inline-flex items-center gap-[0.25rem]">
              {/* Reachability is three-state: absent is UNKNOWN, not offline. */}
              {uplinkReachable === true ? (
                <Wifi className="h-[0.85rem] w-[0.85rem] text-ok" aria-hidden />
              ) : uplinkReachable === false ? (
                <WifiOff className="h-[0.85rem] w-[0.85rem] text-muted-foreground" aria-hidden />
              ) : null}
              {uplink ?? DASH}
            </span>
          </Item>
          <Item label={t("cockpit.shell.strip.cpu")}>{fmtPct(status?.system?.cpu_pct ?? null)}</Item>
        </div>
      ) : null}
    </div>
  );
}

/** RIGHT zone — the box state: pair, temperature, clock. */
function BoxZone({ status, stale }: { status: GsStatus | null; stale: boolean }) {
  const { t } = useTranslation();
  const paired = stripPaired(status);
  const temp = status?.system?.temp_c ?? null;
  const recording = status?.recording === true || status?.video?.recording === true;
  const tempClass = temp == null ? undefined : temp >= 80 ? "text-err" : temp >= 70 ? "text-warn" : undefined;
  const uplinkState = uplinkView(status?.uplink);
  const standby = uplinkState?.state === "standby" ? uplinkState : null;

  return (
    <div className="flex shrink-0 items-center gap-[0.9rem] whitespace-nowrap">
      {recording ? (
        <span className="inline-flex items-center gap-[0.28rem] text-err">
          <span className="h-[0.5rem] w-[0.5rem] animate-pulse rounded-full bg-err" aria-hidden />
          <span className="text-[0.75rem] font-medium">{t("cockpit.shell.strip.rec")}</span>
        </span>
      ) : null}
      {standby ? (
        // A phone holds the uplink: this box only receives, so the kiosk must
        // not read as the screen that flies the drone.
        <span className="inline-flex items-center gap-[0.28rem] rounded-full bg-warn/15 px-[0.42rem] py-[0.08rem] text-warn">
          <span className="text-[0.75rem] font-medium">
            {t("wfb.uplink.stripStandby", { holder: standby.holder ?? t("wfb.uplink.aPhone") })}
          </span>
        </span>
      ) : null}
      <Item label={t("cockpit.shell.strip.pair")}>
        <span className={paired ? "text-ok" : "text-muted-foreground"}>
          {paired ? t("cockpit.shell.strip.bound") : t("cockpit.shell.strip.none")}
        </span>
      </Item>
      <Item label={t("cockpit.shell.strip.temp")}>
        <span className={tempClass}>{fmtTemp(temp)}</span>
      </Item>
      <span className="text-[0.78rem] text-surface-foreground">
        <WallClock />
      </span>
      {stale ? <span className="text-[0.75rem] text-warn">{t("cockpit.shell.strip.stale")}</span> : null}
      <MoreZone status={status} />
    </div>
  );
}

export function StatusStrip() {
  const status = useStatusStore((s) => s.status);
  const stale = useStatusStore((s) => s.stale);

  return (
    <div
      className={cn(
        "relative flex shrink-0 items-center gap-[0.9rem] px-[0.75rem]",
        CHROME.stripHeight,
        "border-b border-border bg-surface/70",
        stale && "opacity-70",
      )}
    >
      <LinkZone link={status?.link} />
      <div className="flex-1" />
      <BoxZone status={status} stale={stale} />
    </div>
  );
}
