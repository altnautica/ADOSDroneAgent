// Uplink — the internet-uplink matrix (Ethernet, WiFi client, 4G modem) with
// per-lane state, the active lane, the failover priority order, the share-uplink
// flag, and the modem data-cap readout. Reads `GET /api/v1/ground-station/network`
// (the lane composite) and `GET /api/v1/ground-station/network/modem` (the modem
// lane + data cap). Read-only — a null field renders a dash, never a fabricated
// zero, so a down lane never masquerades as a live one.

import { useCallback } from "react";
import { useTranslation } from "react-i18next";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { Dot, EmptyNote, ErrorNote, MeterTile, Row, SectionHeader, StaleBadge } from "@/components/ui/data";
import { readFailed } from "@/lib/read-failed";
import type { Tone } from "@/lib/tone";
import { useResource } from "@/shared/use-resource";
import { apiFetch } from "@/shared/api-fetch";
import { DASH } from "@/shared/format";
import { fmtMb } from "@/lib/format-status";
import { laneForToken, uplinkTokenLabel } from "@/lib/uplink-lanes";
import "@/shared/i18n";

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
  const { t } = useTranslation();
  const tone: Tone = active ? "ok" : up ? "warn" : "muted";
  const state = active
    ? t("uplink.state.active")
    : up == null
      ? DASH
      : up
        ? t("uplink.state.connected")
        : t("uplink.state.notConnected");
  return <Row label={name} left={<Dot tone={tone} />} hint={detail} value={right ?? state} tone={tone} />;
}

export function UplinkScreen() {
  const { t } = useTranslation();
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

  // A failed read is unknown, never "no uplink".
  const headline = n == null ? DASH : active ? uplinkTokenLabel(active) : t("uplink.active.none");

  let body;
  if (net.status === 404) {
    body = <EmptyNote>{t("cockpit.uplink.notAvailable")}</EmptyNote>;
  } else if (readFailed(net)) {
    body = <ErrorNote onRetry={net.refresh}>{t("uplink.matrix.loadFailed")}</ErrorNote>;
  } else if (n == null) {
    body = <EmptyNote>{t("common.loading")}</EmptyNote>;
  } else {
    body = (
      <div className="flex flex-col gap-[0.15rem]">
        <SectionHeader>{t("cockpit.uplink.lanes")}</SectionHeader>
        {/* The agent has no live ethernet or USB-tether probe; those lanes are
            known only when they are the active uplink. */}
        <LaneRow name={t("uplink.leg.ethernet")} up={null} active={activeLane === "ethernet"} />
        <LaneRow
          name={t("uplink.leg.wifi")}
          up={typeof wifi?.connected === "boolean" ? wifi.connected : null}
          active={activeLane === "wifi"}
          detail={wifiDetail}
        />
        <LaneRow
          name={t("uplink.leg.cellular")}
          up={typeof m?.connected === "boolean" ? m.connected : null}
          active={activeLane === "modem"}
          detail={modemDetail}
        />
        <LaneRow name={t("uplink.leg.usb")} up={null} active={activeLane === "usb"} />

        {n.priority?.length ? (
          <>
            <SectionHeader>{t("uplink.failover.title")}</SectionHeader>
            <Row label={t("cockpit.uplink.order")} value={n.priority.map(uplinkTokenLabel).join("  ›  ")} mono={false} />
          </>
        ) : null}

        <SectionHeader>{t("cockpit.uplink.sharing")}</SectionHeader>
        <Row
          label={t("uplink.share.title")}
          left={<Dot tone={n.share_uplink ? "ok" : "muted"} />}
          value={n.share_uplink == null ? DASH : n.share_uplink ? t("video.fields.on") : t("video.fields.off")}
        />

        {m?.enabled ? (
          <>
            <SectionHeader>{t("cockpit.uplink.dataCap")}</SectionHeader>
            {capMb && capMb > 0 ? (
              <MeterTile
                label={t("cockpit.uplink.thisPeriod")}
                value={usedMb}
                max={capMb}
                display={`${fmtMb(usedMb)} / ${fmtMb(capMb)}`}
                tone={capPct != null && capPct >= 90 ? "err" : capPct != null && capPct >= 75 ? "warn" : "ok"}
              />
            ) : (
              <Row label={t("cockpit.uplink.used")} value={fmtMb(usedMb)} hint={t("cockpit.uplink.noCap")} />
            )}
            <Row
              label={t("cockpit.uplink.signal")}
              value={m.signal_quality != null && m.signal_quality >= 0 ? `${m.signal_quality}%` : DASH}
            />
            {m.iface ? <Row label={t("cockpit.uplink.iface")} value={m.iface} hint={m.ip ?? undefined} /> : null}
          </>
        ) : null}
      </div>
    );
  }

  return (
    <ConsoleScaffold stale={net.stale}
      title={t("uplink.matrix.title")}
      actions={
        <div className="flex items-center gap-[0.5rem]">
          <Dot tone={active ? "ok" : n == null ? "warn" : "muted"} />
          <span className="text-[0.8rem] text-surface-foreground">{headline}</span>
          <StaleBadge stale={net.stale} />
        </div>
      }
    >
      {body}
    </ConsoleScaffold>
  );
}
