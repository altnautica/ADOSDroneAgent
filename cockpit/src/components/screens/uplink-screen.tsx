// Uplink — the internet-uplink matrix (Ethernet, WiFi client, 4G modem) with
// per-lane state, the active lane, the failover priority order, the share-uplink
// flag, and the modem data-cap readout. Reads `GET /api/v1/ground-station/network`
// (the lane composite) and `GET /api/v1/ground-station/network/modem` (the modem
// lane + data cap). Read-only — a null field renders a dash, never a fabricated
// zero, so a down lane never masquerades as a live one.

import { useCallback } from "react";

import { Panel, PanelHeader } from "@/components/ui/panel";
import { Dot, EmptyNote, MeterTile, Row, SectionHeader, StaleBadge, type Tone } from "@/components/ui/data";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/lib/api";
import { DASH } from "@/lib/format";
import { fmtMb } from "@/lib/format-status";
import { laneForToken, uplinkTokenLabel } from "@/lib/uplink-lanes";

interface WifiClientLane {
  enabled_on_boot?: boolean;
  connected?: boolean;
  ssid?: string | null;
  signal?: number | null;
  ip?: string | null;
}
interface GsNetwork {
  /** Always null on the native front: no live ethernet probe exists, so the
   *  leg is "not probed", never "down". */
  ethernet?: null;
  wifi_client?: WifiClientLane;
  /** The router's interface-role token (eth0 / wlan0_client / wwan0 / usb0). */
  active_uplink?: string | null;
  priority?: string[];
  share_uplink?: boolean;
}
interface ModemLane {
  enabled?: boolean;
  connected?: boolean;
  iface?: string | null;
  ip?: string | null;
  signal_quality?: number | null;
  technology?: string | null;
  apn?: string | null;
  operator?: string | null;
  data_used_mb?: number | null;
  cap_mb?: number | null;
  percent?: number | null;
  state?: string | null;
}

/** A lane row: a status dot, the lane name, an "active" marker on the live
 *  uplink, and the lane's key detail as the hint. `up` is null when the agent
 *  has no reading for the lane, which renders a dash rather than "down". */
function LaneRow({
  name,
  up,
  active,
  detail,
  right,
}: {
  name: string;
  up: boolean | null;
  active: boolean;
  detail?: string;
  right?: string;
}) {
  const tone: Tone = active ? "ok" : up ? "warn" : "muted";
  const state = active ? "active" : up == null ? DASH : up ? "ready" : "down";
  return (
    <Row label={name} left={<Dot tone={tone} />} hint={detail} value={right ?? state} tone={tone} />
  );
}

export function UplinkScreen() {
  const net = useResource<GsNetwork>(
    useCallback((s) => apiFetch<GsNetwork>("/api/v1/ground-station/network", { signal: s }), []),
    2000,
  );
  const modem = useResource<ModemLane>(
    useCallback((s) => apiFetch<ModemLane>("/api/v1/ground-station/network/modem", { signal: s }), []),
    2500,
  );

  const n = net.data;
  const active = n?.active_uplink ?? null;
  const activeLane = laneForToken(active);
  const wifi = n?.wifi_client;
  const m = modem.data;

  const wifiDetail = [wifi?.ssid, wifi?.ip].filter(Boolean).join(" · ") || undefined;
  const modemDetail = [m?.operator, m?.technology, m?.apn].filter((v) => v && v !== "unknown").join(" · ") || undefined;

  const capMb = m?.cap_mb ?? null;
  const usedMb = m?.data_used_mb ?? null;
  const capPct = m?.percent ?? (capMb && usedMb != null ? (usedMb / capMb) * 100 : null);

  return (
    <Panel>
      <PanelHeader
        title="Uplink"
        right={
          <div className="flex items-center gap-[0.5rem]">
            <Dot tone={active ? "ok" : "muted"} />
            <span className="text-[0.8rem] text-surface-foreground">{active ? uplinkTokenLabel(active) : "no uplink"}</span>
            <StaleBadge stale={net.stale} />
          </div>
        }
      />

      {!net.ready && net.data == null ? (
        <EmptyNote>Reading the uplink lanes…</EmptyNote>
      ) : net.status === 404 ? (
        <EmptyNote>The uplink matrix is not available on this profile.</EmptyNote>
      ) : (
        <div className="flex flex-col gap-[0.15rem]">
          <SectionHeader>Lanes</SectionHeader>
          {/* The agent has no live ethernet or USB-tether probe; those lanes are
              known only when they are the active uplink. */}
          <LaneRow name="Ethernet" up={null} active={activeLane === "ethernet"} />
          <LaneRow
            name="WiFi client"
            up={typeof wifi?.connected === "boolean" ? wifi.connected : null}
            active={activeLane === "wifi"}
            detail={wifiDetail}
          />
          <LaneRow
            name="4G modem"
            up={typeof m?.connected === "boolean" ? m.connected : null}
            active={activeLane === "modem"}
            detail={modemDetail}
            right={activeLane === "modem" ? undefined : (m?.state ?? undefined)}
          />
          <LaneRow name="USB tether" up={null} active={activeLane === "usb"} />

          {n?.priority?.length ? (
            <>
              <SectionHeader>Failover priority</SectionHeader>
              <Row label="Order" value={n.priority.map(uplinkTokenLabel).join("  ›  ")} mono={false} />
            </>
          ) : null}

          <SectionHeader>Sharing</SectionHeader>
          <Row
            label="Share uplink"
            left={<Dot tone={n?.share_uplink ? "ok" : "muted"} />}
            hint="NAT the uplink to paired clients"
            value={n?.share_uplink == null ? DASH : n.share_uplink ? "on" : "off"}
          />

          {m?.enabled ? (
            <>
              <SectionHeader>Modem data cap</SectionHeader>
              {capMb && capMb > 0 ? (
                <MeterTile
                  label="This period"
                  value={usedMb}
                  max={capMb}
                  display={`${fmtMb(usedMb)} / ${fmtMb(capMb)}`}
                  tone={capPct != null && capPct >= 90 ? "err" : capPct != null && capPct >= 75 ? "warn" : "ok"}
                />
              ) : (
                <Row label="Used" value={fmtMb(usedMb)} hint="no cap set" />
              )}
              <Row label="Signal quality" value={m?.signal_quality != null && m.signal_quality >= 0 ? `${m.signal_quality}%` : DASH} />
              {m?.iface ? <Row label="Interface" value={m.iface} hint={m?.ip ?? undefined} /> : null}
            </>
          ) : null}
        </div>
      )}
    </Panel>
  );
}
