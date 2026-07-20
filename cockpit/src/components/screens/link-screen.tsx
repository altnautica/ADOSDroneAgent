// Link — the WFB radio link. Reads the rich `GET /api/wfb` status (RSSI / SNR /
// noise / loss / bitrate / FEC / channel / TX power / adapter / regulatory
// domain / dual-check state), the `GET /api/wfb/history` quality trace, and the
// `GET /api/wfb/pair` RF pair state. Read-only: every value comes straight from
// the agent's own producers so the panel can never disagree with the OLED. A
// null reading renders a dash, never a fabricated zero.

import { useCallback } from "react";

import { Panel, PanelHeader } from "@/components/ui/panel";
import {
  Dot,
  EmptyNote,
  Row,
  SectionHeader,
  StaleBadge,
  Tile,
  TileGrid,
  type Tone,
} from "@/components/ui/data";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/lib/api";
import { DASH, fmtChannel, fmtDbm, fmtInt, fmtMbps } from "@/lib/format";
import { fmtDb, fmtKbpsAsMbps, fmtLossPct, fmtMhz } from "@/lib/format-status";
import { cn } from "@/lib/utils";

interface WfbStatus {
  state?: string;
  active?: boolean;
  rssi_dbm?: number | null;
  snr_db?: number | null;
  noise_dbm?: number | null;
  loss_percent?: number | null;
  bitrate_mbps?: number | null;
  bitrate_kbps?: number | null;
  fec_recovered?: number | null;
  fec_failed?: number | null;
  channel?: number | null;
  frequency_mhz?: number | null;
  bandwidth_mhz?: number | null;
  tx_power_dbm?: number | null;
  tx_power_max_dbm?: number | null;
  mcs_index?: number | null;
  packets_received?: number | null;
  packets_lost?: number | null;
  rx_silent_seconds?: number | null;
  restart_count?: number | null;
  adapter?: string | null;
  adapter_chipset?: string | null;
  chipset?: string | null;
  interface?: string | null;
  regulatory_domain?: string | null;
  reg_domain?: string | null;
  adapter_injection_ok?: boolean | null;
  supports_monitor?: boolean | null;
}

interface WfbPair {
  paired?: boolean;
  paired_with_device_id?: string | null;
  paired_at?: string | null;
  fingerprint?: string | null;
  auto_pair_enabled?: boolean;
  role?: string | null;
}

interface HistorySample {
  timestamp?: number;
  rssi_dbm?: number | null;
  rssi?: number | null;
  [k: string]: unknown;
}
interface WfbHistory {
  samples?: HistorySample[];
  count?: number;
}

function linkTone(state: string | undefined, active: boolean | undefined): Tone {
  switch (state) {
    case "connected":
      return "ok";
    case "degraded":
    case "rf_unverified":
      return "warn";
    case "connecting":
      return "muted";
    default:
      return active ? "warn" : "err";
  }
}

/** A tiny inline RSSI sparkline over the recent history samples (best-effort:
 *  reads `rssi_dbm`/`rssi` from whatever the history producer emits). */
function RssiSparkline({ samples }: { samples: HistorySample[] }) {
  const vals = samples
    .map((s) => (typeof s.rssi_dbm === "number" ? s.rssi_dbm : typeof s.rssi === "number" ? s.rssi : null))
    .filter((v): v is number => v != null && Number.isFinite(v));
  if (vals.length < 2) {
    return <EmptyNote>No link-quality history yet.</EmptyNote>;
  }
  const min = Math.min(...vals, -90);
  const max = Math.max(...vals, -30);
  const span = max - min || 1;
  const w = 100;
  const h = 28;
  const pts = vals
    .map((v, i) => {
      const x = (i / (vals.length - 1)) * w;
      const y = h - ((v - min) / span) * h;
      return `${x.toFixed(1)},${y.toFixed(1)}`;
    })
    .join(" ");
  return (
    <svg viewBox={`0 0 ${w} ${h}`} preserveAspectRatio="none" className="h-[2.4rem] w-full">
      <polyline points={pts} fill="none" stroke="hsl(var(--amber))" strokeWidth={1.5} vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

export function LinkScreen() {
  const wfb = useResource<WfbStatus>(useCallback((s) => apiFetch<WfbStatus>("/api/wfb", { signal: s }), []), 700);
  const pair = useResource<WfbPair>(
    useCallback((s) => apiFetch<WfbPair>("/api/wfb/pair", { signal: s }), []),
    2000,
  );
  const history = useResource<WfbHistory>(
    useCallback((s) => apiFetch<WfbHistory>("/api/wfb/history?seconds=60", { signal: s }), []),
    2000,
  );

  const w = wfb.data;
  const tone = linkTone(w?.state, w?.active ?? undefined);
  const chipset = w?.adapter_chipset ?? w?.chipset ?? null;
  const bitrate =
    w?.bitrate_mbps != null ? fmtMbps(w.bitrate_mbps) : fmtKbpsAsMbps(w?.bitrate_kbps);
  const samples = history.data?.samples ?? [];

  return (
    <Panel>
      <PanelHeader
        title="Link"
        right={
          <div className="flex items-center gap-[0.5rem]">
            <Dot tone={tone} />
            <span className={cn("text-[0.8rem]", tone === "ok" ? "text-ok" : tone === "err" ? "text-err" : "text-warn")}>
              {w?.state ?? DASH}
            </span>
            <StaleBadge stale={wfb.stale} />
          </div>
        }
      />

      {!wfb.ready && wfb.data == null ? (
        <EmptyNote>Reading the radio link…</EmptyNote>
      ) : wfb.status === 404 ? (
        <EmptyNote>The WFB radio link is not available on this profile.</EmptyNote>
      ) : (
        <div className="flex flex-col gap-[0.15rem]">
          <SectionHeader>Signal</SectionHeader>
          <TileGrid>
            <Tile label="RSSI" value={fmtDbm(w?.rssi_dbm)} tone={tone} />
            <Tile label="SNR" value={fmtDb(w?.snr_db)} />
            <Tile label="Noise" value={fmtDbm(w?.noise_dbm)} />
            <Tile label="Loss" value={fmtLossPct(w?.loss_percent)} tone={(w?.loss_percent ?? 0) > 5 ? "warn" : undefined} />
            <Tile label="Bitrate" value={bitrate} />
            <Tile label="MCS" value={fmtInt(w?.mcs_index)} />
          </TileGrid>

          <SectionHeader>Channel</SectionHeader>
          <Row label="Channel" value={fmtChannel(w?.channel)} hint={fmtMhz(w?.frequency_mhz)} />
          <Row label="Bandwidth" value={w?.bandwidth_mhz != null ? `${w.bandwidth_mhz} MHz` : DASH} />
          <Row label="TX power" value={fmtDbm(w?.tx_power_dbm)} hint={w?.tx_power_max_dbm != null ? `max ${fmtDbm(w.tx_power_max_dbm)}` : undefined} />
          <Row label="Regulatory domain" value={w?.regulatory_domain ?? w?.reg_domain ?? DASH} />

          <SectionHeader>Packets &amp; FEC</SectionHeader>
          <Row label="Received" value={fmtInt(w?.packets_received)} />
          <Row label="Lost" value={fmtInt(w?.packets_lost)} tone={(w?.packets_lost ?? 0) > 0 ? "warn" : undefined} />
          <Row label="FEC recovered" value={fmtInt(w?.fec_recovered)} />
          <Row label="FEC failed" value={fmtInt(w?.fec_failed)} tone={(w?.fec_failed ?? 0) > 0 ? "err" : undefined} />
          <Row
            label="RX silent"
            value={w?.rx_silent_seconds != null ? `${Math.round(w.rx_silent_seconds)}s` : DASH}
            tone={(w?.rx_silent_seconds ?? 0) > 3 ? "warn" : undefined}
          />

          <SectionHeader>RSSI history (60s)</SectionHeader>
          <div className="rounded-md bg-input/30 px-[0.6rem] py-[0.4rem]">
            <RssiSparkline samples={samples} />
          </div>

          <SectionHeader>Adapter</SectionHeader>
          <Row label="Interface" value={w?.interface ?? DASH} />
          <Row label="Chipset" value={chipset ?? DASH} hint={w?.adapter ?? undefined} />
          <Row
            label="Injection"
            left={<Dot tone={w?.adapter_injection_ok ? "ok" : w?.adapter_injection_ok === false ? "err" : "muted"} />}
            value={w?.adapter_injection_ok == null ? DASH : w.adapter_injection_ok ? "ok" : "no"}
          />
          <Row label="Restarts" value={fmtInt(w?.restart_count)} tone={(w?.restart_count ?? 0) > 0 ? "warn" : undefined} />

          <SectionHeader>Pair</SectionHeader>
          <Row
            label="Paired"
            left={<Dot tone={pair.data?.paired ? "ok" : "muted"} />}
            value={pair.data?.paired ? "linked" : "none"}
            tone={pair.data?.paired ? "ok" : "muted"}
          />
          {pair.data?.paired_with_device_id ? (
            <Row label="Peer" value={pair.data.paired_with_device_id} />
          ) : null}
          {pair.data?.fingerprint ? (
            <Row label="Key" value={pair.data.fingerprint} />
          ) : null}
          <Row
            label="Auto-pair"
            value={pair.data?.auto_pair_enabled == null ? DASH : pair.data.auto_pair_enabled ? "on" : "off"}
          />
        </div>
      )}
    </Panel>
  );
}
