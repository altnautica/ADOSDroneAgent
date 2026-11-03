// The flight controller on the System screen: vehicle, firmware, mode and arm
// state, GPS, battery, RC and the serial port, from the dashboard snapshot. An
// MSP flight controller (Betaflight/iNav) never sends a MAVLink heartbeat, so it
// is shown by its identity instead of as "no FC". Self-polling.

import { useCallback } from "react";
import { useTranslation } from "react-i18next";

import { EmptyNote, Row, SectionHeader } from "@/components/ui/data";
import { fcView } from "@/lib/home-panels";
import { apiFetch } from "@/shared/api-fetch";
import { mspVariantLabel } from "@/shared/api/fc-firmware";
import type { AgentHeartbeat, DashboardSnapshot } from "@/shared/api/types";
import { DASH, fmtNum, fmtPct, fmtVoltage } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

export function FcCard() {
  const { t } = useTranslation();
  const snap = useResource(
    useCallback((s: AbortSignal) => apiFetch<DashboardSnapshot>("/api/v1/dashboard/snapshot", { signal: s }), []),
    2000,
  );
  const hb = useResource(
    useCallback((s: AbortSignal) => apiFetch<AgentHeartbeat>("/api/status", { signal: s }), []),
    5000,
  );
  const fc = snap.data?.fc;
  const view = fcView(fc, snap.data != null, hb.data?.fcVariant);
  const port = fc?.fc_port ? `${fc.fc_port} @ ${fc.fc_baud ?? DASH}` : DASH;

  let body;
  if (view.kind === "connecting") {
    body = <EmptyNote>{t("panels.fc.connecting")}</EmptyNote>;
  } else if (view.kind === "msp") {
    body = (
      <>
        <Row label={t("panels.fc.firmware")} value={mspVariantLabel(view.variant)} mono={false} />
        <Row label={t("panels.fc.telemetry")} value={t("panels.fc.viaMsp")} mono={false} />
        {fc?.fc_port ? <Row label={t("panels.fc.port")} value={port} /> : null}
      </>
    );
  } else if (view.kind === "absent") {
    body = (
      <EmptyNote>
        {t("panels.fc.noFcConnected")}
        {fc?.fc_port ? ` · ${t("panels.fc.port")} ${fc.fc_port}` : ""}
      </EmptyNote>
    );
  } else {
    const armed = fc?.armed === true ? t("panels.fc.armed") : fc?.armed === false ? t("panels.fc.disarmed") : t("panels.fc.armStateUnknown");
    body = (
      <>
        {view.waiting ? <Row label={t("panels.fc.telemetry")} value={t("panels.fc.waitingTelemetry")} tone="warn" mono={false} /> : null}
        <Row label={t("panels.fc.vehicle")} value={fc?.vehicle || DASH} />
        <Row label={t("panels.fc.firmware")} value={fc?.firmware || DASH} />
        <Row label={t("panels.fc.mode")} value={`${(fc?.mode || DASH).toUpperCase()} · ${armed}`} tone={fc?.armed ? "warn" : undefined} />
        <Row
          label={t("panels.fc.gps")}
          value={
            fc?.gps.satellites_visible != null
              ? t("panels.fc.gpsValue", { sats: fc.gps.satellites_visible, hdop: fmtNum(fc.gps.hdop, 1) })
              : DASH
          }
        />
        <Row
          label={t("panels.fc.battery")}
          value={fc?.battery.voltage != null ? `${fmtVoltage(fc.battery.voltage)} · ${fmtPct(fc.battery.remaining)}` : DASH}
        />
        <Row label={t("panels.fc.rc")} value={fmtPct(fc?.rc)} />
        <Row label={t("panels.fc.port")} value={port} />
      </>
    );
  }

  return (
    <div className="flex flex-col gap-[0.15rem]">
      <SectionHeader>{t("panels.fc.title")}</SectionHeader>
      {body}
    </div>
  );
}
