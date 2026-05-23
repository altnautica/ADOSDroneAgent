import {
  type LucideIcon,
  Cloud,
  Cpu,
  KeyRound,
  Radio,
  Video,
  Wifi,
} from "lucide-react";

import { summarizeHardware } from "@/components/panels/hardware-item-list";
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
  title?: string;
}

function tiles(
  snap: ReturnType<typeof useSnapshot>,
  status: ReturnType<typeof useStatus>,
): Tile[] {
  const s = snap.data;
  const cfg = status.data;

  // MAV — tile reflects MAVLink health. Sats render as the primary
  // value once GPS reports something; before that the tile shows the
  // FC link state so a connected-but-unlocked rig does not look dead.
  const fcConnected = s?.fc.connected ?? false;
  const sats = s?.fc.gps.satellites_visible ?? null;
  const mav: Tile = {
    label: "MAV",
    icon: Radio,
    value:
      sats != null && sats > 0
        ? String(sats)
        : fcConnected
          ? "linked"
          : "off",
    sub: fcConnected
      ? sats != null && sats > 0
        ? "sats"
        : "waiting for fix"
      : "no fc",
    severity: fcConnected ? "ok" : "idle",
  };

  // HW — required-component summary from /api/v1/setup/status
  const hwItems = cfg?.hardware_check?.items ?? [];
  const hwSum = summarizeHardware(hwItems);
  const hwSev: Severity =
    hwItems.length === 0
      ? "idle"
      : hwSum.worstState === "missing"
        ? "err"
        : hwSum.worstState === "warning" || hwSum.worstState === "checking"
          ? "warn"
          : "ok";
  const failingNames = hwItems
    .filter((i) => i.required && i.state !== "ok")
    .map((i) => i.label)
    .join(", ");
  const hw: Tile = {
    label: "HW",
    icon: Cpu,
    value:
      hwItems.length === 0
        ? "—"
        : `${hwSum.requiredOk}/${hwSum.requiredTotal}`,
    sub: hwItems.length === 0 ? "scanning" : failingNames || "all required ok",
    severity: hwSev,
    title: failingNames ? `Failing required: ${failingNames}` : undefined,
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

  // CLD — primary value is the operator's chosen cloud posture;
  // mqtt / http details only appear when a relay is supposed to be
  // dialing out. Local-mode rigs render as "local" rather than the
  // stale "unknown" of the runtime probe.
  const cloudMode = s?.cloud.mode ?? cfg?.cloud_choice?.mode ?? "local";
  const mqtt = s?.cloud.mqtt_state ?? "unknown";
  const http = s?.cloud.http_state ?? "unknown";
  let cldSub: string;
  let cldSeverity: Severity;
  if (cloudMode === "local") {
    cldSub = "no relay";
    cldSeverity = "idle";
  } else if (mqtt === "connected" || mqtt === "online") {
    cldSub = http !== "unknown" ? `http ${http}` : "online";
    cldSeverity = "ok";
  } else if (mqtt === "unknown") {
    cldSub = "connecting";
    cldSeverity = "idle";
  } else {
    cldSub = `mqtt ${mqtt}`;
    cldSeverity = "warn";
  }
  const cld: Tile = {
    label: "CLD",
    icon: Cloud,
    value: cloudMode,
    sub: cldSub,
    severity: cldSeverity,
  };

  // PAIR — in local mode there's nothing to pair with; the tile reads
  // "n/a · local mode" instead of inheriting the historical paired
  // state from setup-status.
  const code = s?.cloud.pairing_code ?? "";
  const finalized = cfg?.setup_finalized ?? false;
  let pair: Tile;
  if (cloudMode === "local") {
    pair = {
      label: "PAIR",
      icon: KeyRound,
      value: "n/a",
      sub: "local mode",
      severity: "idle",
    };
  } else {
    pair = {
      label: "PAIR",
      icon: KeyRound,
      value: code ? code : finalized ? "paired" : "—",
      sub: code ? "code rotates" : finalized ? "linked" : "unpaired",
      severity: code ? "info" : finalized ? "ok" : "idle",
    };
  }

  return [mav, hw, vid, net, cld, pair];
}

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

export function StatusTiles() {
  const snap = useSnapshot();
  const status = useStatus();
  const isLoading = snap.isLoading && !snap.data;
  const ts = tiles(snap, status);

  return (
    <div className="grid grid-cols-2 sm:grid-cols-3 lg:grid-cols-6 gap-3">
      {ts.map((t) => (
        <StatusTile key={t.label} tile={t} isLoading={isLoading} />
      ))}
    </div>
  );
}
