// The Home status-tile row as a pure function of the data it reads, so the
// tile rules (FC link, pairing) are unit-testable without React.

import { Cloud, Cpu, KeyRound, Radio, Video, Wifi, type LucideIcon } from "lucide-react";

import { mspVariant } from "./fc-firmware";
import { summarizeHardware } from "./hardware-summary";
import type { DashboardSnapshot, SetupStatus } from "./types";
import { fmtBitrate, severityFromState, type Severity } from "@/shared/format";

export interface Tile {
  label: string;
  icon: LucideIcon;
  value: string;
  sub: string;
  severity: Severity;
  title?: string;
}

export interface TileInputs {
  snapshot: DashboardSnapshot | undefined;
  status: SetupStatus | undefined;
  /** Heartbeat (`/api/status`) FC identity. */
  fcVariant: string | null | undefined;
  fcReachable: boolean | null | undefined;
  /** `/api/pairing/info`: the node's real cloud-pair state. Undefined until read. */
  pairing: { paired: boolean; pairing_code: string | null } | undefined;
}

function fcTile(snap: DashboardSnapshot | undefined, fcVariant: TileInputs["fcVariant"], fcReachable: TileInputs["fcReachable"]): Tile {
  const fcConnected = snap?.fc?.connected ?? false;
  const variant = mspVariant(fcVariant);
  // A Betaflight/iNav FC speaks MSP (no MAVLink heartbeat), so `fc.connected`
  // stays false. The tile is "linked" only while the agent reports the FC
  // reachable; the variant alone (which can outlive the transport) is sub-text.
  if (!fcConnected && variant) {
    const reachable = fcReachable === true;
    return {
      label: "FC",
      icon: Radio,
      value: reachable ? "linked" : "off",
      sub: variant === "betaflight" ? "betaflight · msp" : "inav · msp",
      severity: reachable ? "ok" : "idle",
    };
  }
  const sats = snap?.fc?.gps?.satellites_visible ?? null;
  const hasFix = sats != null && sats > 0;
  return {
    label: "MAV",
    icon: Radio,
    value: hasFix ? String(sats) : fcConnected ? "linked" : "off",
    sub: fcConnected ? (hasFix ? "sats" : "waiting for fix") : "no fc",
    severity: fcConnected ? "ok" : "idle",
  };
}

function hwTile(status: SetupStatus | undefined): Tile {
  const items = status?.hardware_check?.items ?? [];
  const sum = summarizeHardware(items);
  const severity: Severity =
    items.length === 0
      ? "idle"
      : sum.worstState === "missing"
        ? "err"
        : sum.worstState === "warning" || sum.worstState === "checking"
          ? "warn"
          : "ok";
  const failing = items
    .filter((i) => i.required && i.state !== "ok")
    .map((i) => i.label)
    .join(", ");
  return {
    label: "HW",
    icon: Cpu,
    value: items.length === 0 ? "—" : `${sum.requiredOk}/${sum.requiredTotal}`,
    sub: items.length === 0 ? "scanning" : failing || "all required ok",
    severity,
    title: failing ? `Failing required: ${failing}` : undefined,
  };
}

function pairTile(cloudMode: string | null, code: string, pairing: TileInputs["pairing"]): Tile {
  // In local mode there is nothing to pair with.
  if (cloudMode === "local") {
    return { label: "PAIR", icon: KeyRound, value: "n/a", sub: "local mode", severity: "idle" };
  }
  if (pairing?.paired === true) {
    return { label: "PAIR", icon: KeyRound, value: "paired", sub: "linked", severity: "ok" };
  }
  if (pairing?.paired === false) {
    return code
      ? { label: "PAIR", icon: KeyRound, value: code, sub: "code rotates", severity: "info" }
      : { label: "PAIR", icon: KeyRound, value: "—", sub: "unpaired", severity: "idle" };
  }
  // Pairing state not read yet: claim nothing.
  return { label: "PAIR", icon: KeyRound, value: "—", sub: "—", severity: "idle" };
}

export function buildTiles({ snapshot: s, status: cfg, fcVariant, fcReachable, pairing }: TileInputs): Tile[] {
  const vState = s?.video?.state ?? "unknown";
  const vBitrate = s?.video?.bitrate_kbps ?? 0;
  const vid: Tile = {
    label: "VID",
    icon: Video,
    value: vBitrate > 0 ? fmtBitrate(vBitrate) : vState,
    sub: vBitrate > 0 ? "live" : vState,
    severity: severityFromState(vState),
  };

  const uplink = cfg?.network?.uplink_kind ?? "—";
  const rssi = cfg?.network?.rssi_dbm ?? null;
  const net: Tile = {
    label: "NET",
    icon: Wifi,
    value: typeof uplink === "string" ? uplink : "—",
    sub: rssi != null ? `${rssi} dBm` : uplink && uplink !== "—" ? "online" : "—",
    severity: uplink && uplink !== "—" ? "ok" : "idle",
  };

  // CLD — the operator's chosen cloud posture. The relay's live link state is
  // not reported to this dashboard, so the tile states the posture only.
  const cloudMode = s?.cloud?.mode ?? cfg?.cloud_choice?.mode ?? null;
  const cld: Tile = {
    label: "CLD",
    icon: Cloud,
    value: cloudMode ?? "—",
    sub: cloudMode === "local" ? "no relay" : cloudMode ? "relay configured" : "—",
    severity: "idle",
  };

  const code = pairing?.pairing_code || s?.cloud?.pairing_code || "";
  return [
    fcTile(s, fcVariant, fcReachable),
    hwTile(cfg),
    vid,
    net,
    cld,
    pairTile(cloudMode, code, pairing),
  ];
}
