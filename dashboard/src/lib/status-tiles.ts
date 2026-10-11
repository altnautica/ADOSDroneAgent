// The Home status-tile row as a pure function of the data it reads, so the
// tile rules (FC link, pairing) are unit-testable without React.

import { Cloud, Cpu, KeyRound, Radio, Video, Wifi, type LucideIcon } from "lucide-react";

import i18n from "@/i18n";

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
      label: i18n.t("panels.tiles.fc"),
      icon: Radio,
      value: reachable ? i18n.t("panels.tiles.linked") : i18n.t("panels.tiles.off"),
      sub: variant === "betaflight" ? i18n.t("panels.tiles.betaflightMsp") : i18n.t("panels.tiles.inavMsp"),
      severity: reachable ? "ok" : "idle",
    };
  }
  const sats = snap?.fc?.gps?.satellites_visible ?? null;
  const hasFix = sats != null && sats > 0;
  return {
    label: i18n.t("panels.tiles.mav"),
    icon: Radio,
    value: hasFix ? String(sats) : fcConnected ? i18n.t("panels.tiles.linked") : i18n.t("panels.tiles.off"),
    sub: fcConnected ? (hasFix ? i18n.t("panels.tiles.sats") : i18n.t("panels.tiles.waitingForFix")) : i18n.t("panels.tiles.noFc"),
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
    label: i18n.t("panels.tiles.hw"),
    icon: Cpu,
    value: items.length === 0 ? "—" : `${sum.requiredOk}/${sum.requiredTotal}`,
    sub: items.length === 0 ? i18n.t("panels.tiles.scanning") : failing || i18n.t("panels.tiles.allRequiredOk"),
    severity,
    title: failing ? i18n.t("panels.tiles.failingRequired", { failing }) : undefined,
  };
}

function pairTile(cloudMode: string | null, code: string, pairing: TileInputs["pairing"]): Tile {
  const label = i18n.t("panels.tiles.pair");
  // LAN pairing works in every cloud mode, so a paired node reads paired first.
  if (pairing?.paired === true) {
    return { label, icon: KeyRound, value: i18n.t("panels.tiles.paired"), sub: i18n.t("panels.tiles.linked"), severity: "ok" };
  }
  // In local mode with no pairing code there is nothing to show.
  if (cloudMode === "local" && !code) {
    return { label, icon: KeyRound, value: i18n.t("panels.tiles.notApplicable"), sub: i18n.t("panels.tiles.localMode"), severity: "idle" };
  }
  if (pairing?.paired === false) {
    return code
      ? { label, icon: KeyRound, value: code, sub: i18n.t("panels.tiles.codeRotates"), severity: "info" }
      : { label, icon: KeyRound, value: "—", sub: i18n.t("panels.tiles.unpaired"), severity: "idle" };
  }
  // Pairing state not read yet: claim nothing.
  return { label, icon: KeyRound, value: "—", sub: "—", severity: "idle" };
}

export function buildTiles({ snapshot: s, status: cfg, fcVariant, fcReachable, pairing }: TileInputs): Tile[] {
  const vState = s?.video?.state ?? "unknown";
  const vBitrate = s?.video?.bitrate_kbps ?? 0;
  const vid: Tile = {
    label: i18n.t("panels.tiles.vid"),
    icon: Video,
    value: vBitrate > 0 ? fmtBitrate(vBitrate) : vState,
    sub: vBitrate > 0 ? i18n.t("panels.tiles.live") : vState,
    severity: severityFromState(vState),
  };

  const uplink = cfg?.network?.uplink_kind ?? "—";
  const rssi = cfg?.network?.rssi_dbm ?? null;
  const net: Tile = {
    label: i18n.t("panels.tiles.net"),
    icon: Wifi,
    value: typeof uplink === "string" ? uplink : "—",
    sub: rssi != null ? `${rssi} dBm` : uplink && uplink !== "—" ? i18n.t("panels.tiles.online") : "—",
    severity: uplink && uplink !== "—" ? "ok" : "idle",
  };

  // CLD — the operator's chosen cloud posture. The relay's live link state is
  // not reported to this dashboard, so the tile states the posture only.
  const cloudMode = s?.cloud?.mode ?? cfg?.cloud_choice?.mode ?? null;
  const cld: Tile = {
    label: i18n.t("panels.tiles.cld"),
    icon: Cloud,
    value: cloudMode ?? "—",
    sub: cloudMode === "local" ? i18n.t("panels.tiles.noRelay") : cloudMode ? i18n.t("panels.tiles.relayConfigured") : "—",
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
