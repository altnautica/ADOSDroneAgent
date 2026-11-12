import { ChevronDown } from "lucide-react";
import { useMemo } from "react";
import { useTranslation } from "react-i18next";

import { BATTERY_POLL_MS, useBatteryHealth } from "@/hooks/use-battery-health";
import { navPilot, useFlightChip } from "@/hooks/use-flight-chip";
import { HEARTBEAT_POLL_MS, useHeartbeat } from "@/hooks/use-heartbeat";
import { useNavStatus } from "@/hooks/use-nav-status";
import { useNow } from "@/hooks/use-now";
import { PAIRING_POLL_MS, usePairingInfo } from "@/hooks/use-pairing";
import { useResource } from "@/hooks/use-resource";
import { SNAPSHOT_POLL_MS, useSnapshot } from "@/hooks/use-snapshot";
import { STATUS_POLL_MS, useStatus } from "@/hooks/use-status";
import { useVehicleLiveness } from "@/hooks/use-vehicle-liveness";
import { useVehicleTelemetry, VEHICLE_TELEMETRY_POLL_MS } from "@/hooks/use-vehicle-telemetry";
import { liveData } from "@/lib/freshness";
import { chipTextColor } from "@/lib/chip-color";
import { freshNavFrames, homePill, homeStatusLines, poseAgeText, type StatusFull } from "@/lib/home-status";
import { buildTiles, type Tile } from "@/lib/status-tiles";
import { cn } from "@/lib/utils";
import { severityClasses } from "@/shared/format";
import { specText } from "@/shared/hud/spec-text";

const STATUS_FULL_POLL_MS = 5_000;
const AGL_LABEL = specText("agl.source.rangefinder");
const RESERVE_TITLE = specText("cockpit.battery.reserve");

function StatusTile({ tile, isLoading }: { tile: Tile; isLoading: boolean }) {
  const Icon = tile.icon;
  const sev = severityClasses(tile.severity);

  return (
    <div
      className="rounded-lg border border-border bg-card p-3 flex flex-col gap-1"
      title={tile.title}
    >
      <div className="flex items-center justify-between text-[11px] font-medium uppercase tracking-wider text-muted-foreground">
        <span className="inline-flex items-center gap-1.5">
          <Icon className={cn("h-3 w-3", sev.text)} />
          {tile.label}
        </span>
        <span className={cn("h-1.5 w-1.5 rounded-full", sev.dot)} />
      </div>
      <div className="font-mono text-lg font-medium leading-tight">
        {isLoading ? "…" : tile.value}
      </div>
      <div className="text-xs text-muted-foreground font-mono truncate">
        {isLoading ? "—" : tile.sub}
      </div>
    </div>
  );
}

/** Who flies the aircraft now, read-only (the cockpit's flight-state chip). */
function FlightChip() {
  const { t } = useTranslation();
  const chip = useFlightChip();
  if (!chip) return null;
  return (
    <div className="flex min-w-0 flex-wrap items-center gap-2 text-xs" aria-label={t("home.flightState")}>
      <span
        className={cn("max-w-full truncate rounded-full border px-2.5 py-0.5 font-mono font-semibold", chip.dim && "opacity-60")}
        style={{ color: chipTextColor(chip.color), borderColor: chip.color }}
        title={chip.text}
        data-testid="flight-chip"
      >
        {chip.text}
      </span>
      {chip.secondary.map((l) => (
        <span key={l.text} className="max-w-full truncate font-mono" style={{ color: chipTextColor(l.color) }} title={l.text}>
          {l.text}
        </span>
      ))}
    </div>
  );
}

const PILL_TONE = {
  ok: "border-ok/50 text-ok",
  warning: "border-warn/60 text-warn",
  critical: "border-destructive/60 text-destructive",
} as const;

/** The cockpit's status pill and its alert list, plus the link-loss, AGL,
 *  sticks and time-to-reserve lines. Read-only: nothing here acknowledges. */
function VehicleStatus() {
  const nav = useNavStatus();
  const heartbeat = useHeartbeat();
  const tel = useVehicleTelemetry();
  const battery = useBatteryHealth();
  const statusFull = useResource<StatusFull>("status-full", "/api/status/full", STATUS_FULL_POLL_MS);
  const now = useNow(1_000);
  const liveness = useVehicleLiveness(tel, VEHICLE_TELEMETRY_POLL_MS, now);
  // The pill reads the kept pilot frame: its age turns it into one
  // "not answering" item.
  const pilot = useMemo(() => navPilot(nav.data), [nav.data]);
  const inputs = {
    pilot,
    telemetry: tel.data ?? null,
    battery: battery.data ?? null,
    status: statusFull.data ?? null,
    live: liveness.live,
    msSinceLive: liveness.msSinceLive,
    armedAtLastLive: liveness.armedAtLastLive,
    pilotAgeMs: nav.data?.pilot && pilot ? Math.max(0, now - nav.dataUpdatedAt) + (nav.data.pilot.age_ms ?? 0) : null,
    agentReachable: !heartbeat.isError,
  };
  // The lines are live values: each reads only a source that is still live,
  // and the pilot, follow and nav frames only while fresh.
  const fresh = freshNavFrames(nav.data, nav.dataUpdatedAt, now);
  const liveTelemetry = liveData(tel, VEHICLE_TELEMETRY_POLL_MS, now) ?? null;
  const pill = homePill({ ...inputs, nav: fresh.nav, armSources: { pilot: fresh.pilot, telemetry: liveTelemetry } });
  const poseAge = poseAgeText(fresh.nav);
  const lines = homeStatusLines({
    ...inputs,
    pilot: fresh.pilot,
    follow: fresh.follow,
    telemetry: liveTelemetry,
    battery: liveData(battery, BATTERY_POLL_MS, now) ?? null,
  });
  return (
    <div className="flex flex-col gap-1 text-xs" data-testid="vehicle-status">
      <details className="group">
        <summary
          className={cn(
            "inline-flex max-w-full cursor-pointer list-none items-center gap-1.5 rounded-full border px-2.5 py-0.5 font-medium max-md:min-h-11",
            PILL_TONE[pill.severity],
          )}
          data-severity={pill.severity}
          data-testid="status-pill"
        >
          <span className="truncate" title={pill.text}>{pill.text}</span>
          {pill.count > 0 ? <ChevronDown aria-hidden className="h-3 w-3 transition-transform group-open:rotate-180" /> : null}
        </summary>
        {pill.count > 0 ? (
          <ul className="mt-1.5 flex flex-col gap-0.5 pl-1" data-testid="status-alerts">
            {pill.alerts.map((a) => (
              <li key={a.id} className="flex items-baseline gap-2">
                <span className={cn("h-1.5 w-1.5 shrink-0 rounded-full", a.severity === "critical" ? "bg-destructive" : "bg-warn")} />
                <span className="text-muted-foreground">{specText(`cockpit.status.source.${a.source}`)}</span>
                <span>{a.text}</span>
              </li>
            ))}
          </ul>
        ) : null}
      </details>
      <div className="flex flex-wrap items-center gap-x-4 gap-y-0.5 font-mono text-muted-foreground">
        {lines.linkLoss ? <span className="text-warn">{lines.linkLoss}</span> : null}
        {lines.agl ? (
          <span>
            {AGL_LABEL} {lines.agl.text}
            {lines.agl.mark ? <span className="ml-1 text-warn">{lines.agl.mark}</span> : null}
          </span>
        ) : null}
        {lines.sticks ? <span>{lines.sticks}</span> : null}
        {poseAge ? <span data-testid="pose-age">{poseAge}</span> : null}
        {lines.timeToReserve ? (
          <span title={RESERVE_TITLE}>{lines.timeToReserve}</span>
        ) : null}
      </div>
    </div>
  );
}

export function StatusTiles() {
  const snap = useSnapshot();
  const status = useStatus();
  const heartbeat = useHeartbeat();
  const pairing = usePairingInfo();
  const now = useNow(1_000);
  const isLoading = snap.isLoading && !snap.data;
  // Each tile's source shows only while live; a failed or late read dashes it.
  const hb = liveData(heartbeat, HEARTBEAT_POLL_MS, now);
  const ts = buildTiles({
    snapshot: liveData(snap, SNAPSHOT_POLL_MS, now),
    status: liveData(status, STATUS_POLL_MS, now),
    fcVariant: hb?.fcVariant,
    fcReachable: hb?.fcReachable,
    pairing: liveData(pairing, PAIRING_POLL_MS, now),
  });

  return (
    <div className="flex flex-col gap-3">
      <FlightChip />
      <VehicleStatus />
      <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-6 gap-3">
        {ts.map((t) => (
          <StatusTile key={t.label} tile={t} isLoading={isLoading} />
        ))}
      </div>
    </div>
  );
}
