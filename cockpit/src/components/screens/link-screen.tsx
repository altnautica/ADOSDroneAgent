// Link — the WFB radio link. Reads the rich `GET /api/wfb` status (RSSI / SNR /
// noise / loss / bitrate / FEC / channel / TX power / adapter / regulatory
// domain / dual-check state), the `GET /api/wfb/history` quality trace, and the
// `GET /api/wfb/pair` RF pair state. Read-only: every value comes straight from
// the agent's own producers so the panel can never disagree with the OLED. A
// null reading renders a dash, never a fabricated zero.

import { useCallback } from "react";

import { VerdictChip } from "@/components/shell/status-strip";
import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { Dot, EmptyNote, ErrorNote, Row, SectionHeader, StaleBadge, Tile, TileGrid } from "@/components/ui/data";
import { readFailed } from "@/lib/read-failed";
import { useResource } from "@/shared/use-resource";
import { apiFetch } from "@/shared/api-fetch";
import { DASH, fmtChannel, fmtDbm, fmtInt, fmtMbps } from "@/shared/format";
import { fmtDb, fmtKbpsAsMbps, fmtLossPct, fmtMhz } from "@/lib/format-status";
import { adapterDetail, linkDiagView } from "@/lib/link-diag";
import { cn } from "@/lib/utils";
import { useTranslation } from "react-i18next";
import "@/shared/i18n";
import { linkKeyStatusView } from "@/shared/link-key-status";
import { useProfile } from "@/shared/use-profile";
import { UplinkHandoverSection } from "./uplink-handover-section";
import { linkStateKey, linkTone, regionLabelKey } from "./link-state";
import { VideoReceiveSection } from "./video-receive-section";

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
  // The one-glance diagnosis + the RX counters that separate the failure modes
  // a bare "0 received" hides. `fanout_forwarded` is ground-station-only (the
  // receiver relaying decoded packets into the video pipeline).
  link_diag?: string | null;
  packets_all?: number | null;
  decrypt_errors?: number | null;
  packets_bad?: number | null;
  session_packets?: number | null;
  fanout_forwarded?: number | null;
  fanout_drops?: number | null;
  /** The agent sends an OBJECT here (`{chipset, driver, supports_monitor}`),
   *  not a string. It was typed as a string and rendered directly, which threw
   *  "objects are not valid as a React child" and took the whole Link screen
   *  down on visit — a hand-written type that disagreed with the wire, with
   *  nothing to catch the disagreement. Typed as it actually is, and read
   *  through [`adapterDetail`]. */
  adapter?: { chipset?: string | null; driver?: string | null } | string | null;
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

/** A tiny inline RSSI sparkline over the recent history samples (best-effort:
 *  reads `rssi_dbm`/`rssi` from whatever the history producer emits). */
function RssiSparkline({ samples }: { samples: HistorySample[] }) {
  const { t } = useTranslation();
  const vals = samples
    .map((s) => (typeof s.rssi_dbm === "number" ? s.rssi_dbm : typeof s.rssi === "number" ? s.rssi : null))
    .filter((v): v is number => v != null && Number.isFinite(v));
  if (vals.length < 2) {
    return <EmptyNote>{t("cockpit.link.noHistory")}</EmptyNote>;
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
      <polyline points={pts} fill="none" stroke="hsl(var(--primary))" strokeWidth={1.5} vectorEffect="non-scaling-stroke" />
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
  // Ground side only: which key the presence/hop control plane authenticates under.
  const isGroundStation = useProfile() === "ground_station";
  const videoConfig = useResource<unknown>(
    useCallback((s) => apiFetch<unknown>("/api/video/config", { signal: s }), []),
    5000,
    { enabled: isGroundStation },
  );
  const keyStatus = isGroundStation ? linkKeyStatusView(videoConfig.data) : null;
  const { t } = useTranslation();

  const w = wfb.data;
  const adapterHint = adapterDetail(w?.adapter);
  const tone = linkTone(w?.state, w?.active ?? undefined);
  const diag = linkDiagView(w?.link_diag);
  const chipset = w?.adapter_chipset ?? w?.chipset ?? null;
  const bitrate =
    w?.bitrate_mbps != null ? fmtMbps(w.bitrate_mbps) : fmtKbpsAsMbps(w?.bitrate_kbps);
  const samples = history.data?.samples ?? [];

  return (
    <ConsoleScaffold stale={wfb.stale}
      title={t("cockpit.link.title")}
      actions={
          // The verdict names the state exactly as the status strip does, so
          // one link state has one name and one colour; the bare state word
          // stands in only while the agent has not classified the link.
          <div className="flex items-center gap-[0.5rem]">
            <Dot tone={diag ? diag.tone : tone} />
            {diag ? (
              <VerdictChip view={diag} />
            ) : (
              <span className={cn("text-[0.8rem]", tone === "ok" ? "text-ok" : tone === "err" ? "text-err" : "text-warn")}>
                {w?.state ? t(linkStateKey(w.state)) : DASH}
              </span>
            )}
            <StaleBadge stale={wfb.stale} />
          </div>
        }
    >
      {wfb.status === 404 ? (
        <EmptyNote>{t("cockpit.link.notAvailable")}</EmptyNote>
      ) : readFailed(wfb) ? (
        <ErrorNote onRetry={wfb.refresh}>{t("cockpit.readFailed.wfb")}</ErrorNote>
      ) : wfb.data == null ? (
        <EmptyNote>{t("common.loading")}</EmptyNote>
      ) : (
        <div className="flex flex-col gap-[0.15rem]">
          {isGroundStation ? <UplinkHandoverSection /> : null}
          {isGroundStation ? <VideoReceiveSection /> : null}
          <SectionHeader>{t("cockpit.link.diagnosis")}</SectionHeader>
          {diag ? (
            <Row
              label={t("cockpit.link.verdict")}
              left={<Dot tone={diag.tone} />}
              value={diag.label}
              tone={diag.tone}
              hint={diag.hint}
            />
          ) : (
            <Row label={t("cockpit.link.verdict")} value={DASH} hint={t("cockpit.link.notClassified")} />
          )}
          <Row
            label={t("cockpit.link.framesSeen")}
            value={w?.packets_all != null ? t("cockpit.link.perSecond", { n: fmtInt(w.packets_all) }) : DASH}
            hint={t("cockpit.link.framesSeenHint")}
            tone={w?.link_diag === "deaf" ? "warn" : undefined}
          />
          <Row
            label={t("cockpit.link.decryptErrors")}
            value={w?.decrypt_errors != null ? t("cockpit.link.perSecond", { n: fmtInt(w.decrypt_errors) }) : DASH}
            hint={t("cockpit.link.decryptErrorsHint")}
            tone={(w?.decrypt_errors ?? 0) > 0 ? "warn" : undefined}
          />
          {w?.fanout_forwarded != null ? (
            <Row
              label={t("cockpit.link.forwarded")}
              value={t("cockpit.link.total", { n: fmtInt(w.fanout_forwarded) })}
              hint={t("cockpit.link.forwardedHint")}
            />
          ) : null}

          <SectionHeader>{t("cockpit.link.signal")}</SectionHeader>
          <TileGrid>
            <Tile label={t("cockpit.link.rssi")} value={fmtDbm(w?.rssi_dbm)} tone={tone} />
            <Tile label={t("cockpit.link.snr")} value={fmtDb(w?.snr_db)} />
            <Tile label={t("cockpit.link.noise")} value={fmtDbm(w?.noise_dbm)} />
            <Tile label={t("cockpit.link.loss")} value={fmtLossPct(w?.loss_percent)} tone={(w?.loss_percent ?? 0) > 5 ? "warn" : undefined} />
            <Tile label={t("cockpit.link.bitrate")} value={bitrate} />
            <Tile label={t("cockpit.link.mcs")} value={fmtInt(w?.mcs_index)} />
          </TileGrid>

          <SectionHeader>{t("cockpit.link.channelSection")}</SectionHeader>
          <Row label={t("cockpit.link.channel")} value={fmtChannel(w?.channel)} hint={fmtMhz(w?.frequency_mhz)} />
          <Row label={t("cockpit.link.bandwidth")} value={fmtMhz(w?.bandwidth_mhz)} />
          <Row
            label={t("cockpit.link.txPower")}
            value={fmtDbm(w?.tx_power_dbm)}
            hint={w?.tx_power_max_dbm != null ? t("cockpit.link.txPowerMax", { dbm: fmtDbm(w.tx_power_max_dbm) }) : undefined}
          />
          <Row
            label={t("cockpit.link.regDomain")}
            value={(() => {
              const domain = w?.regulatory_domain ?? w?.reg_domain ?? null;
              const key = regionLabelKey(domain);
              return key ? t(key) : (domain ?? DASH);
            })()}
          />

          <SectionHeader>{t("cockpit.link.packets")}</SectionHeader>
          <Row label={t("cockpit.link.received")} value={fmtInt(w?.packets_received)} />
          <Row label={t("cockpit.link.lost")} value={fmtInt(w?.packets_lost)} tone={(w?.packets_lost ?? 0) > 0 ? "warn" : undefined} />
          <Row label={t("cockpit.link.fecRecovered")} value={fmtInt(w?.fec_recovered)} />
          <Row label={t("cockpit.link.fecFailed")} value={fmtInt(w?.fec_failed)} tone={(w?.fec_failed ?? 0) > 0 ? "err" : undefined} />
          <Row
            label={t("cockpit.link.rxSilent")}
            value={w?.rx_silent_seconds != null ? t("cockpit.link.seconds", { s: Math.round(w.rx_silent_seconds) }) : DASH}
            tone={(w?.rx_silent_seconds ?? 0) > 3 ? "warn" : undefined}
          />

          <SectionHeader>{t("cockpit.link.history")}</SectionHeader>
          <div className="rounded-md bg-input/30 px-[0.6rem] py-[0.4rem]">
            <RssiSparkline samples={samples} />
          </div>

          <SectionHeader>{t("cockpit.link.adapter")}</SectionHeader>
          <Row label={t("cockpit.link.iface")} value={w?.interface ?? DASH} />
          <Row label={t("cockpit.link.chipset")} value={chipset ?? DASH} hint={adapterHint} />
          <Row
            label={t("cockpit.link.injection")}
            left={<Dot tone={w?.adapter_injection_ok ? "ok" : w?.adapter_injection_ok === false ? "err" : "muted"} />}
            value={w?.adapter_injection_ok == null ? DASH : w.adapter_injection_ok ? t("cockpit.link.injectionOk") : t("cockpit.link.injectionFailed")}
          />
          <Row label={t("cockpit.link.restarts")} value={fmtInt(w?.restart_count)} tone={(w?.restart_count ?? 0) > 0 ? "warn" : undefined} />

          <SectionHeader>{t("cockpit.link.pair")}</SectionHeader>
          <Row
            label={t("cockpit.link.paired")}
            left={<Dot tone={pair.data == null ? "warn" : pair.data.paired ? "ok" : "muted"} />}
            value={pair.data == null ? DASH : pair.data.paired ? t("cockpit.pair.bound") : t("cockpit.link.notPaired")}
            tone={pair.data?.paired ? "ok" : "muted"}
          />
          {pair.data?.paired_with_device_id ? (
            <Row label={t("cockpit.link.peer")} value={pair.data.paired_with_device_id} />
          ) : null}
          {pair.data?.fingerprint ? (
            <Row label={t("cockpit.link.fingerprint")} value={pair.data.fingerprint} />
          ) : null}
          {keyStatus ? (
            <Row
              label={t("cockpit.link.linkKey")}
              value={t(keyStatus.labelKey)}
              tone={keyStatus.variant === "warn" ? "warn" : undefined}
            />
          ) : null}
          <Row
            label={t("cockpit.link.autoPair")}
            value={pair.data?.auto_pair_enabled == null ? DASH : pair.data.auto_pair_enabled ? t("video.fields.on") : t("video.fields.off")}
          />
        </div>
      )}
    </ConsoleScaffold>
  );
}
