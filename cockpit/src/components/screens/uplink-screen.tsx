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

interface EthernetLane {
  link?: boolean;
  speed_mbps?: number | null;
  ip?: string | null;
  gateway?: string | null;
}
interface WifiClientLane {
  enabled_on_boot?: boolean;
  connected?: boolean;
  ssid?: string | null;
  signal?: number | null;
  ip?: string | null;
}
interface GsNetwork {
  ethernet?: EthernetLane;
  wifi_client?: WifiClientLane;
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

const LANE_LABEL: Record<string, string> = {
  ethernet: "Ethernet",
  wifi_client: "WiFi client",
  wifi: "WiFi client",
  modem: "4G modem",
  cellular: "4G modem",
};

function laneLabel(key: string): string {
  return LANE_LABEL[key] ?? key;
}

/** A lane row: a status dot, the lane name, an "active" marker on the live
 *  uplink, and the lane's key detail (IP / SSID / speed) as the hint. */
function LaneRow({
  name,
  up,
  active,
  detail,
  right,
}: {
  name: string;
  up: boolean;
  active: boolean;
  detail?: string;
  right?: string;
}) {
  const tone: Tone = active ? "ok" : up ? "warn" : "muted";
  return (
    <Row
      label={name}
      left={<Dot tone={tone} />}
      hint={detail}
      value={right ?? (active ? "active" : up ? "ready" : "down")}
      tone={tone}
    />
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
  const eth = n?.ethernet;
  const wifi = n?.wifi_client;
  const m = modem.data;

  const ethIp = eth?.ip ?? undefined;
  const ethSpeed = eth?.speed_mbps != null ? `${eth.speed_mbps} Mbps` : undefined;
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
            <span className="text-[0.8rem] text-surface-foreground">{active ? laneLabel(active) : "no uplink"}</span>
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
          <LaneRow
            name="Ethernet"
            up={Boolean(eth?.link)}
            active={active === "ethernet"}
            detail={[ethIp, ethSpeed].filter(Boolean).join(" · ") || undefined}
          />
          <LaneRow
            name="WiFi client"
            up={Boolean(wifi?.connected)}
            active={active === "wifi_client" || active === "wifi"}
            detail={wifiDetail}
          />
          <LaneRow
            name="4G modem"
            up={Boolean(m?.connected)}
            active={active === "modem" || active === "cellular"}
            detail={modemDetail}
            right={m?.state ?? undefined}
          />

          {n?.priority?.length ? (
            <>
              <SectionHeader>Failover priority</SectionHeader>
              <Row label="Order" value={n.priority.map(laneLabel).join("  ›  ")} mono={false} />
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
