import { type LucideIcon, Radio, Video, Wifi, Cloud, KeyRound } from "lucide-react";

import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";
import { fmtBitrate, severityClasses, severityFromState } from "@/lib/format";
import type { Severity } from "@/lib/types";
import { cn } from "@/lib/utils";

interface Tile {
  label: string;
  icon: LucideIcon;
  value: string;
  sub: string;
  severity: Severity;
}

function tiles(
  snap: ReturnType<typeof useSnapshot>,
  status: ReturnType<typeof useStatus>,
): Tile[] {
  const s = snap.data;
  const cfg = status.data;

  // MAV
  const fcConnected = s?.fc.connected ?? false;
  const sats = s?.fc.gps.satellites_visible ?? null;
  const mav: Tile = {
    label: "MAV",
    icon: Radio,
    value: sats != null ? String(sats) : fcConnected ? "—" : "off",
    sub: fcConnected ? `${sats != null ? "sats" : "no gps"}` : "no fc",
    severity: fcConnected ? "ok" : "idle",
  };

  // VID
  const vState = s?.video.state ?? "unknown";
  const vBitrate = s?.video.bitrate_kbps ?? 0;
  const vid: Tile = {
    label: "VID",
    icon: Video,
    value: vBitrate > 0 ? fmtBitrate(vBitrate) : vState,
    sub: vBitrate > 0 ? "live" : vState,
    severity: severityFromState(vState),
  };

  // NET
  const uplink = s?.network?.uplink ?? cfg?.network?.uplink_kind ?? "—";
  const rssi =
    typeof s?.network?.rssi_dbm === "number"
      ? s?.network?.rssi_dbm
      : (cfg?.network?.rssi_dbm ?? null);
  const net: Tile = {
    label: "NET",
    icon: Wifi,
    value: typeof uplink === "string" ? uplink : "—",
    sub: rssi != null ? `${rssi} dBm` : uplink && uplink !== "—" ? "online" : "—",
    severity: uplink && uplink !== "—" ? "ok" : "idle",
  };

  // CLD
  const mqtt = s?.cloud.mqtt_state ?? "unknown";
  const http = s?.cloud.http_state ?? "unknown";
  const cldOk = mqtt === "connected" || mqtt === "online";
  const cld: Tile = {
    label: "CLD",
    icon: Cloud,
    value: cldOk ? "online" : mqtt,
    sub: http !== "unknown" ? `http ${http}` : "—",
    severity: cldOk ? "ok" : mqtt === "unknown" ? "idle" : "warn",
  };

  // PAIR
  const code = s?.cloud.pairing_code ?? "";
  const finalized = cfg?.setup_finalized ?? false;
  const pair: Tile = {
    label: "PAIR",
    icon: KeyRound,
    value: code ? code : finalized ? "paired" : "—",
    sub: code ? "code rotates" : finalized ? "linked" : "unpaired",
    severity: code ? "info" : finalized ? "ok" : "idle",
  };

  return [mav, vid, net, cld, pair];
}

function StatusTile({ tile, isLoading }: { tile: Tile; isLoading: boolean }) {
  const Icon = tile.icon;
  const sev = severityClasses(tile.severity);

  return (
    <div className="rounded-lg border border-border bg-card p-3 flex flex-col gap-1">
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

export function StatusTiles() {
  const snap = useSnapshot();
  const status = useStatus();
  const isLoading = snap.isLoading && !snap.data;
  const ts = tiles(snap, status);

  return (
    <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-5 gap-3">
      {ts.map((t) => (
        <StatusTile key={t.label} tile={t} isLoading={isLoading} />
      ))}
    </div>
  );
}
